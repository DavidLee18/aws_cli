//! Listing a prefix, in parallel where the keyspace allows it.
//!
//! `ListObjectsV2` returns at most 1000 keys and a continuation token, and that chain is
//! strictly sequential: page N+1 cannot be requested until page N comes back. Listing
//! 100,000 objects is therefore 100 round trips one after another, and on a link with any
//! latency that dominates everything else — parsing the XML is a rounding error beside it.
//!
//! The keyspace itself is the way out. A listing with `delimiter=/` returns the immediate
//! sub-prefixes, and each of those can be walked on its own continuation chain,
//! independently and at the same time. A bucket laid out as `p/000/…`, `p/001/…` turns 100
//! sequential trips into a handful of concurrent ones.
//!
//! This only helps when the keys have structure. A flat prefix has no sub-prefixes to
//! split on, so [`deep`] falls back to walking sequentially, which is what it would have
//! done anyway.

use super::conn::Conn;
use super::xml;
use crate::exit;
use crate::Failure;
use aws_cli_runtime::http;
use std::sync::Mutex;

/// Below this many sub-prefixes, fanning out costs more in extra requests than it saves.
const MIN_SHARDS: usize = 2;

/// One listed object, in the form both the transfer planner and `ls` need.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub size: u64,
    pub last_modified: String,
}

/// What a single `delimiter=/` listing found: the immediate sub-prefixes, and the objects
/// sitting directly under the prefix rather than in one of them.
pub struct Shallow {
    pub prefixes: Vec<String>,
    pub direct: Vec<Entry>,
}

fn list_query(prefix: &str, delimiter: Option<&str>, token: Option<&str>) -> String {
    // `encoding-type=url` so a key containing characters XML cannot carry survives the
    // listing; the fields are decoded again as they are read.
    let mut query = vec![
        "list-type=2".to_string(),
        "encoding-type=url".to_string(),
        format!("prefix={}", super::encode_query(prefix)),
    ];
    if let Some(d) = delimiter {
        query.push(format!("delimiter={}", super::encode_query(d)));
    }
    if let Some(t) = token {
        query.push(format!("continuation-token={}", super::encode_query(t)));
    }
    query.join("&")
}

fn entries_from(root: &xml::Element, out: &mut Vec<Entry>) {
    for content in root.all("Contents") {
        out.push(Entry {
            key: super::decode_listed(content.get("Key")),
            size: content.get("Size").parse().unwrap_or_default(),
            last_modified: content.get("LastModified").to_string(),
        });
    }
}

/// One level of the keyspace under `prefix`.
pub fn shallow(conn: &Conn, prefix: &str) -> Result<Shallow, Failure> {
    let mut prefixes = Vec::new();
    let mut direct = Vec::new();
    let mut token: Option<String> = None;

    loop {
        let query = list_query(prefix, Some("/"), token.as_deref());
        let response =
            conn.send_checked("ListObjectsV2", "GET", "/", &query, &[], http::Body::Empty)?;
        let root =
            xml::parse(&response.text()).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        for common in root.all("CommonPrefixes") {
            prefixes.push(super::decode_listed(common.get("Prefix")));
        }
        entries_from(&root, &mut direct);

        match root.get("NextContinuationToken") {
            "" => break,
            next => token = Some(next.to_string()),
        }
    }
    Ok(Shallow { prefixes, direct })
}

/// Walk `prefix` and everything beneath it on a single continuation chain.
pub fn sequential(conn: &Conn, prefix: &str) -> Result<Vec<Entry>, Failure> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let query = list_query(prefix, None, token.as_deref());
        let response =
            conn.send_checked("ListObjectsV2", "GET", "/", &query, &[], http::Body::Empty)?;
        let root =
            xml::parse(&response.text()).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        entries_from(&root, &mut out);
        match root.get("NextContinuationToken") {
            "" => break,
            next => token = Some(next.to_string()),
        }
    }
    Ok(out)
}

/// Every object under `prefix`, fanning out over sub-prefixes when there are enough of
/// them to be worth it.
///
/// Results come back in key order regardless of the order the shards finish in.
pub fn deep(conn: &Conn, prefix: &str, workers: usize) -> Result<Vec<Entry>, Failure> {
    let top = shallow(conn, prefix)?;
    if top.prefixes.len() < MIN_SHARDS {
        // Nothing to split on. One extra request was spent finding that out, against a
        // walk that would have been sequential either way.
        return sequential(conn, prefix);
    }

    let mut out = top.direct;
    let failure: Mutex<Option<Failure>> = Mutex::new(None);
    let results: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

    // Bounded fan-out: `workers` shards in flight at a time, so a bucket with thousands of
    // sub-prefixes does not try to open a connection for every one of them.
    for chunk in top.prefixes.chunks(workers.max(1)) {
        std::thread::scope(|scope| {
            for shard in chunk {
                scope.spawn(|| match sequential(conn, shard) {
                    Ok(entries) => results.lock().expect("mutex").extend(entries),
                    Err(e) => {
                        let mut slot = failure.lock().expect("mutex");
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                    }
                });
            }
        });
        if let Some(e) = failure.lock().expect("mutex").take() {
            return Err(e);
        }
    }

    out.extend(results.into_inner().expect("mutex"));
    // Shards finish out of order, and callers -- `ls` above all -- expect key order.
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}


/// Every object under `prefix`, in key order, handed to `emit` in batches as it arrives.
///
/// The buffering variant [`deep`] holds the whole listing, which is fine for a transfer
/// plan but not for `ls` on a bucket with millions of keys. Here memory stays bounded by
/// one chunk of shards plus the objects sitting directly under `prefix`:
///
///   - sub-prefixes are sorted, so every key in one chunk of shards sorts before every key
///     in the next; chunks can be emitted and dropped as they complete.
///   - objects directly under `prefix` are *not* confined to any chunk (`p/zzz.txt` sorts
///     after `p/000/…`), so they are held aside and merged in by key as the stream
///     advances. There is one of those per non-directory entry at this level, which is the
///     part of the listing that does not grow with depth.
pub fn deep_streaming(
    conn: &Conn,
    prefix: &str,
    workers: usize,
    mut emit: impl FnMut(&[Entry]) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let top = shallow(conn, prefix)?;
    if top.prefixes.len() < MIN_SHARDS {
        let mut all = sequential(conn, prefix)?;
        all.sort_by(|a, b| a.key.cmp(&b.key));
        return emit(&all);
    }

    let mut direct = top.direct;
    direct.sort_by(|a, b| a.key.cmp(&b.key));
    let mut pending = direct.into_iter().peekable();

    for chunk in top.prefixes.chunks(workers.max(1)) {
        let failure: Mutex<Option<Failure>> = Mutex::new(None);
        let results: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for shard in chunk {
                scope.spawn(|| match sequential(conn, shard) {
                    Ok(entries) => results.lock().expect("mutex").extend(entries),
                    Err(e) => {
                        let mut slot = failure.lock().expect("mutex");
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                    }
                });
            }
        });
        if let Some(e) = failure.into_inner().expect("mutex") {
            return Err(e);
        }

        let mut batch = results.into_inner().expect("mutex");
        batch.sort_by(|a, b| a.key.cmp(&b.key));

        // Merge in every held-aside direct child that falls within this chunk's key
        // range. Flushing only those sorting *before* the chunk is not enough: a child
        // like `p/000012zz.bin` sits between two shards that are both inside this chunk,
        // so it has to be interleaved, not appended.
        //
        // An empty batch consumes nothing -- those children belong to a later chunk.
        let merged = match batch.last().map(|e| e.key.clone()) {
            None => Vec::new(),
            Some(chunk_end) => {
                let mut within: Vec<Entry> = Vec::new();
                while pending.peek().is_some_and(|e| e.key <= chunk_end) {
                    within.push(pending.next().expect("peeked"));
                }
                merge_by_key(within, batch)
            }
        };
        if !merged.is_empty() {
            emit(&merged)?;
        }
    }

    // Whatever sorts after the last shard.
    let tail: Vec<Entry> = pending.collect();
    if !tail.is_empty() {
        emit(&tail)?;
    }
    Ok(())
}


/// Merge two key-sorted runs into one.
fn merge_by_key(a: Vec<Entry>, b: Vec<Entry>) -> Vec<Entry> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut a = a.into_iter().peekable();
    let mut b = b.into_iter().peekable();
    loop {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => x.key <= y.key,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        out.push(if take_a {
            a.next().expect("peeked")
        } else {
            b.next().expect("peeked")
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str) -> Entry {
        Entry { key: key.to_string(), size: 1, last_modified: String::new() }
    }

    fn keys(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.key.as_str()).collect()
    }

    /// The case that actually broke: a direct child sorting *between* two shards, not
    /// before or after all of them.
    #[test]
    fn interleaves_direct_children_with_shard_results() {
        let direct = vec![entry("p/000012zz.bin")];
        let shards = vec![entry("p/000012/a"), entry("p/000013/a")];
        assert_eq!(
            keys(&merge_by_key(direct, shards)),
            ["p/000012/a", "p/000012zz.bin", "p/000013/a"]
        );
    }

    #[test]
    fn merges_leading_and_trailing_runs() {
        assert_eq!(
            keys(&merge_by_key(vec![entry("a"), entry("z")], vec![entry("m")])),
            ["a", "m", "z"]
        );
        assert_eq!(keys(&merge_by_key(Vec::new(), vec![entry("m")])), ["m"]);
        assert_eq!(keys(&merge_by_key(vec![entry("m")], Vec::new())), ["m"]);
    }

    /// Equal keys cannot happen across these two runs -- a key is either directly under
    /// the prefix or inside a shard -- but the merge must still be total.
    #[test]
    fn handles_equal_keys() {
        assert_eq!(keys(&merge_by_key(vec![entry("a")], vec![entry("a")])), ["a", "a"]);
    }
}
