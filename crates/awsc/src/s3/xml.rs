//! Just enough XML reading for the S3 responses the `s3` tree issues.
//!
//! The generic parser in `aws-cli-protocol` works from a modelled shape; these requests
//! are hand-built and have no shape to hand it, so this walks the document directly.

use quick_xml::events::Event;
use quick_xml::Reader;

/// One element: its name, its text, and its children.
#[derive(Debug, Default, Clone)]
pub struct Element {
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    /// Direct children with this name.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// The text of a direct child, or empty when absent.
    pub fn get(&self, name: &str) -> &str {
        self.child(name).map(|c| c.text.as_str()).unwrap_or_default()
    }
}

pub fn parse(body: &str) -> Result<Element, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<Element> = vec![Element::default()];
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                stack.push(Element { name, ..Default::default() });
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    let done = stack.pop().expect("stack has more than one frame");
                    stack.last_mut().expect("root frame is never popped").children.push(done);
                }
            }
            // A self-closing element carries no text but may still need to be present.
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                stack
                    .last_mut()
                    .expect("root frame is never popped")
                    .children
                    .push(Element { name, ..Default::default() });
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().map_err(|e| e.to_string())?.into_owned();
                stack.last_mut().expect("root frame is never popped").text.push_str(&text);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("malformed XML from S3: {e}")),
            _ => {}
        }
        buf.clear();
    }

    let root = stack.remove(0);
    // The document element is the single child of the synthetic root.
    Ok(root.children.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_list_objects_response() {
        let body = r#"<?xml version="1.0"?>
        <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
          <Name>bucket</Name>
          <IsTruncated>false</IsTruncated>
          <Contents><Key>a.txt</Key><Size>12</Size>
            <LastModified>2026-08-13T21:48:16.000Z</LastModified></Contents>
          <Contents><Key>b.txt</Key><Size>0</Size>
            <LastModified>2026-08-13T21:48:17.000Z</LastModified></Contents>
          <CommonPrefixes><Prefix>sub/</Prefix></CommonPrefixes>
        </ListBucketResult>"#;
        let root = parse(body).unwrap();
        // The namespace is stripped, leaving the local name.
        assert_eq!(root.name, "ListBucketResult");
        assert_eq!(root.get("Name"), "bucket");
        let contents: Vec<&Element> = root.all("Contents").collect();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].get("Key"), "a.txt");
        assert_eq!(contents[1].get("Size"), "0");
        assert_eq!(root.child("CommonPrefixes").unwrap().get("Prefix"), "sub/");
        // An absent child reads as empty rather than failing.
        assert_eq!(root.get("NextContinuationToken"), "");
    }

    #[test]
    fn reports_malformed_documents() {
        assert!(parse("<a><b></a>").is_err());
    }
}
