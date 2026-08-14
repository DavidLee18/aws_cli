//! `aws s3 sync`.
//!
//! A sorted merge-join over the two sides, keyed on the path relative to each root. Both
//! listings are produced in byte order so the join is a single pass.
//!
//! The comparison rules come straight from the reference and two of them are easy to get
//! backwards:
//!
//! - **The time test is asymmetric.** An upload is skipped when the destination is at
//!   least as new as the source (`dest - src >= 0`); a download is skipped when the
//!   *local* file is no newer than the object (`dest - src <= 0`). Downloads stamp the
//!   local mtime to the object's `LastModified` afterwards, which is what makes a clean
//!   download settle at exactly zero.
//! - **There is no tolerance.** Not a second, not a millisecond. The comparison is exact.

use super::transfer::{self, Item, Options, Verb};
use super::{param_error, uri::Location};
use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use std::process::ExitCode;

/// Which side an unmatched entry came from, which decides what happens to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    /// Present at the source only: always transferred.
    SourceOnly,
    /// Present at both: transferred only if the strategy says so.
    Both,
    /// Present at the destination only: deleted, but only under `--delete`.
    DestOnly,
}

/// How two entries that exist on both sides are compared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    /// Size, then the asymmetric time test.
    SizeAndTime,
    /// `--size-only`: timestamps are never consulted.
    SizeOnly,
    /// `--exact-timestamps`: downloads additionally require the times to match exactly.
    ExactTimestamps,
}

pub fn run(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let (mut options, paths) = Options::parse_for_sync(parsed)?;
    if paths.len() < 2 {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            format!(
                "{}\n\n{}",
                aws_cli_runtime::RuntimeError::ParamValidation(
                    "the following arguments are required: paths".to_string()
                ),
                crate::USAGE_HINT
            ),
        ));
    }
    if paths.len() > 2 {
        return Err(param_error(format!("Unknown options: {}", paths[2..].join(","))));
    }
    // sync always walks whole trees.
    options.recursive = true;

    let source = Location::parse(&paths[0]).map_err(param_error)?;
    let dest = Location::parse(&paths[1]).map_err(param_error)?;

    if let Location::Local(path) = &source {
        if !std::path::Path::new(path).exists() {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!("The user-provided path {path} does not exist."),
            ));
        }
    }

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    match (&source, &dest) {
        (Location::Local(local), Location::S3 { bucket, key }) => {
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = transfer::Conn::from_client(&client, globals);
            let root = transfer::abspath(local);
            let mut left = transfer::scan_local(local, true, options.follow_symlinks)?;
            left.retain(|i| transfer::included(&i.source, &root, &options.excludes));
            let right = transfer::scan_s3(&conn, &transfer::dir_prefix(key))?;
            let plan = plan(&left, &right, options.strategy, Verb::Sync, true, options.delete);
            transfer::sync_upload(&conn, plan, key, &options, bucket)
        }
        (Location::S3 { bucket, key }, Location::Local(local)) => {
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = transfer::Conn::from_client(&client, globals);
            let root = format!("{bucket}/{}", key.trim_end_matches('/'));
            let mut left = transfer::scan_s3(&conn, &transfer::dir_prefix(key))?;
            left.retain(|i| {
                transfer::included(&format!("{bucket}/{}", i.source), &root, &options.excludes)
            });
            let right = transfer::scan_local_if_present(local)?;
            let plan = plan(&left, &right, options.strategy, Verb::Sync, false, options.delete);
            transfer::sync_download(&conn, plan, local, &options, bucket)
        }
        (
            Location::S3 { bucket: sb, key: sk },
            Location::S3 { bucket: db, key: dk },
        ) => {
            let client = Client::for_bucket(&model, globals, Some(db))?;
            let conn = transfer::Conn::from_client(&client, globals);
            let source_client = Client::for_bucket(&model, globals, Some(sb))?;
            let source_conn = transfer::Conn::from_client(&source_client, globals);
            let root = format!("{sb}/{}", sk.trim_end_matches('/'));
            let mut left = transfer::scan_s3(&source_conn, &transfer::dir_prefix(sk))?;
            left.retain(|i| {
                transfer::included(&format!("{sb}/{}", i.source), &root, &options.excludes)
            });
            let right = transfer::scan_s3(&conn, &transfer::dir_prefix(dk))?;
            let plan = plan(&left, &right, options.strategy, Verb::Sync, true, options.delete);
            transfer::sync_copy(&conn, &source_conn, plan, dk, &options, sb)
        }
        _ => Err(param_error(
            "usage: aws s3 sync <LocalPath> <S3Uri> or <S3Uri> <LocalPath> or \
             <S3Uri> <S3Uri>\nError: Invalid argument type",
        )),
    }
}

/// One decided action.
pub struct Action {
    pub item: Item,
    pub delete: bool,
}

/// Merge the two sorted listings and decide what to do with each key.
///
/// Both sides must already be sorted by `dest` (their key relative to the root); the scan
/// functions guarantee it. Actions come out in key order, so deletes are interleaved with
/// transfers rather than forming a separate phase — which is what the reference does.
fn plan(
    source: &[Item],
    dest: &[Item],
    strategy: Strategy,
    _verb: Verb,
    uploading: bool,
    delete: bool,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);

    while i < source.len() || j < dest.len() {
        let side = match (source.get(i), dest.get(j)) {
            (Some(s), Some(d)) => match s.dest.cmp(&d.dest) {
                std::cmp::Ordering::Equal => Side::Both,
                std::cmp::Ordering::Less => Side::SourceOnly,
                std::cmp::Ordering::Greater => Side::DestOnly,
            },
            (Some(_), None) => Side::SourceOnly,
            (None, Some(_)) => Side::DestOnly,
            (None, None) => break,
        };

        match side {
            Side::SourceOnly => {
                let s = &source[i];
                actions.push(Action {
                    item: Item {
                        source: s.source.clone(),
                        dest: s.dest.clone(),
                        size: s.size,
                        modified: s.modified,
                    },
                    delete: false,
                });
                i += 1;
            }
            Side::Both => {
                let (s, d) = (&source[i], &dest[j]);
                if should_sync(s, d, strategy, uploading) {
                    actions.push(Action {
                        item: Item {
                            source: s.source.clone(),
                            dest: s.dest.clone(),
                            size: s.size,
                            modified: s.modified,
                        },
                        delete: false,
                    });
                }
                i += 1;
                j += 1;
            }
            Side::DestOnly => {
                if delete {
                    let d = &dest[j];
                    actions.push(Action {
                        item: Item {
                            source: d.source.clone(),
                            dest: d.dest.clone(),
                            size: d.size,
                            modified: d.modified,
                        },
                        delete: true,
                    });
                }
                j += 1;
            }
        }
    }
    actions
}

/// Whether an entry present on both sides needs transferring.
fn should_sync(source: &Item, dest: &Item, strategy: Strategy, uploading: bool) -> bool {
    let same_size = source.size == dest.size;
    match strategy {
        // Timestamps are never consulted.
        Strategy::SizeOnly => !same_size,
        _ => !same_size || !same_time(source, dest, strategy, uploading),
    }
}

/// The asymmetric time test. `true` means "no update needed".
fn same_time(source: &Item, dest: &Item, strategy: Strategy, uploading: bool) -> bool {
    let delta = dest.modified - source.modified;
    if uploading {
        // Upload or copy: skip when the destination is at least as new.
        delta >= 0.0
    } else if strategy == Strategy::ExactTimestamps {
        // --exact-timestamps tightens ONLY the download case to an exact match, so a
        // local file older than the object now triggers a download.
        delta == 0.0
    } else {
        // Download: skip when the local file is no newer than the object.
        delta <= 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, size: u64, modified: f64) -> Item {
        Item { source: key.to_string(), dest: key.to_string(), size, modified }
    }

    fn keys(actions: &[Action]) -> Vec<(String, bool)> {
        actions.iter().map(|a| (a.item.dest.clone(), a.delete)).collect()
    }

    /// A source-only key is always transferred; a destination-only key is left alone
    /// unless --delete is set.
    #[test]
    fn transfers_new_and_ignores_extra_without_delete() {
        let source = vec![item("a", 1, 100.0)];
        let dest = vec![item("b", 1, 100.0)];
        assert_eq!(
            keys(&plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, true, false)),
            [("a".to_string(), false)]
        );
    }

    /// With --delete the extra destination key is removed, and the actions come out in
    /// key order rather than as separate phases.
    #[test]
    fn interleaves_deletes_in_key_order() {
        let source = vec![item("a", 1, 100.0), item("c", 1, 100.0)];
        let dest = vec![item("b", 1, 100.0)];
        assert_eq!(
            keys(&plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, true, true)),
            [("a".to_string(), false), ("b".to_string(), true), ("c".to_string(), false)]
        );
    }

    /// A differing size always transfers, whatever the timestamps say.
    #[test]
    fn size_difference_always_transfers() {
        let source = vec![item("a", 2, 100.0)];
        let dest = vec![item("a", 1, 999.0)];
        for strategy in [Strategy::SizeAndTime, Strategy::SizeOnly, Strategy::ExactTimestamps] {
            assert_eq!(
                keys(&plan(&source, &dest, strategy, Verb::Sync, true, false)).len(),
                1,
                "{strategy:?}"
            );
        }
    }

    /// Uploads skip when the destination is at least as new — including exactly equal.
    #[test]
    fn upload_skips_when_destination_is_not_older() {
        let source = vec![item("a", 1, 100.0)];
        for dest_time in [100.0, 101.0] {
            let dest = vec![item("a", 1, dest_time)];
            assert!(
                plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, true, false).is_empty(),
                "dest at {dest_time} should not upload"
            );
        }
        // Source strictly newer: upload.
        let dest = vec![item("a", 1, 99.0)];
        assert_eq!(plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, true, false).len(), 1);
    }

    /// Downloads run the test the other way: skip when the local copy is no newer.
    #[test]
    fn download_skips_when_local_is_not_newer() {
        let source = vec![item("a", 1, 100.0)]; // the S3 object
        for local_time in [100.0, 99.0] {
            let dest = vec![item("a", 1, local_time)];
            assert!(
                plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, false, false).is_empty(),
                "local at {local_time} should not download"
            );
        }
        // A local file NEWER than the object does trigger a download — the direction that
        // surprises people, and it is correct.
        let dest = vec![item("a", 1, 101.0)];
        assert_eq!(plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, false, false).len(), 1);
    }

    /// --exact-timestamps tightens downloads only: anything but an exact match transfers.
    #[test]
    fn exact_timestamps_requires_equality_on_download() {
        let source = vec![item("a", 1, 100.0)];
        let equal = vec![item("a", 1, 100.0)];
        assert!(plan(&source, &equal, Strategy::ExactTimestamps, Verb::Sync, false, false)
            .is_empty());
        // Older local file: skipped by default, transferred with --exact-timestamps.
        let older = vec![item("a", 1, 99.0)];
        assert!(plan(&source, &older, Strategy::SizeAndTime, Verb::Sync, false, false).is_empty());
        assert_eq!(
            plan(&source, &older, Strategy::ExactTimestamps, Verb::Sync, false, false).len(),
            1
        );
        // Uploads are unaffected by the flag.
        let newer_dest = vec![item("a", 1, 101.0)];
        assert!(plan(&source, &newer_dest, Strategy::ExactTimestamps, Verb::Sync, true, false)
            .is_empty());
    }

    /// --size-only never looks at time, in either direction.
    #[test]
    fn size_only_ignores_timestamps() {
        let source = vec![item("a", 1, 100.0)];
        for dest_time in [1.0, 100.0, 10_000.0] {
            let dest = vec![item("a", 1, dest_time)];
            for uploading in [true, false] {
                assert!(
                    plan(&source, &dest, Strategy::SizeOnly, Verb::Sync, uploading, false)
                        .is_empty(),
                    "time {dest_time} uploading={uploading}"
                );
            }
        }
    }

    /// The merge is a single pass over both sorted lists and must not lose entries.
    #[test]
    fn merges_long_runs_without_losing_keys() {
        let source: Vec<Item> = (0..50).map(|n| item(&format!("k{n:03}"), 1, 100.0)).collect();
        let dest: Vec<Item> =
            (0..50).filter(|n| n % 2 == 0).map(|n| item(&format!("k{n:03}"), 9, 100.0)).collect();
        let actions = plan(&source, &dest, Strategy::SizeAndTime, Verb::Sync, true, true);
        // Every source key transfers: the odd ones are new, the even ones differ in size.
        assert_eq!(actions.len(), 50);
        assert!(actions.iter().all(|a| !a.delete));
    }
}
