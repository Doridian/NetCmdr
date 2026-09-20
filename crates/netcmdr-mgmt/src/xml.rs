// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small, lossless XML element tree.
//!
//! Changing a setting on the device is a read-modify-write of its *whole*
//! configuration document: the vendor's client fetches `config_data`, lets its
//! dialogs mutate the object graph, and posts the entire thing back (see
//! `SaveCfgHttpCmd`). Anything the client drops on the way is dropped on the
//! device too, so the one safe way to change a single attribute is to hand the
//! rest of the document back exactly as it arrived.
//!
//! That is what this module is for. It keeps attribute order, keeps attributes
//! and elements it has no model for, and does not rewrite what it does not
//! understand — unlike [`crate::model`], which parses the same document into
//! typed Rust for display and deliberately forgets the rest.
//!
//! It is not a general XML implementation: no namespaces, no DTDs, no
//! processing instructions. The device emits none of those.

use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};

use crate::Error;

/// A node in the tree: an element, or the text between elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// One XML element, with its attributes in the order they were written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(name: impl Into<String>) -> Self {
        Element {
            name: name.into(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    /// An attribute's value, or `None` if the attribute is absent.
    ///
    /// The firmware embeds stray newlines inside some values (`name="D1214403\n"`),
    /// so this trims; [`Element::set`] writes back untrimmed.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
    }

    /// An attribute's value, or `""` when absent.
    pub fn attr_or_empty(&self, name: &str) -> &str {
        self.attr(name).unwrap_or("")
    }

    /// `"true"`/`"1"` → true; absent or anything else → false.
    pub fn attr_bool(&self, name: &str) -> bool {
        matches!(self.attr(name), Some(v) if v.eq_ignore_ascii_case("true") || v == "1")
    }

    pub fn attr_parsed<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.attr(name).and_then(|v| v.parse().ok())
    }

    /// The text (and CDATA) directly inside this element, concatenated.
    pub fn text(&self) -> String {
        self.children
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Set an attribute, keeping its position if it is already there so that a
    /// modified document still reads like the one the device sent.
    pub fn set(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        let value = value.into();
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => *v = value,
            None => self.attrs.push((name.to_string(), value)),
        }
        self
    }

    /// Set an attribute to `"true"` or `"false"`, the spelling the PV schema uses.
    pub fn set_bool(&mut self, name: &str, value: bool) -> &mut Self {
        self.set(name, if value { "true" } else { "false" })
    }

    pub fn remove_attr(&mut self, name: &str) {
        self.attrs.retain(|(k, _)| k != name);
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children_named(name).next()
    }

    pub fn child_mut(&mut self, name: &str) -> Option<&mut Element> {
        self.children_named_mut(name).next()
    }

    pub fn children_named<'s, 'n>(
        &'s self,
        name: &'n str,
    ) -> impl Iterator<Item = &'s Element> + use<'s, 'n> {
        self.children.iter().filter_map(move |n| match n {
            Node::Element(e) if e.name == name => Some(e),
            _ => None,
        })
    }

    pub fn children_named_mut<'s, 'n>(
        &'s mut self,
        name: &'n str,
    ) -> impl Iterator<Item = &'s mut Element> + use<'s, 'n> {
        self.children.iter_mut().filter_map(move |n| match n {
            Node::Element(e) if e.name == name => Some(e),
            _ => None,
        })
    }

    /// The first `name` child whose `attr` equals `value`.
    pub fn child_where(&self, name: &str, attr: &str, value: &str) -> Option<&Element> {
        self.children_named(name)
            .find(|e| e.attr(attr) == Some(value))
    }

    /// The first `name` child whose `attr` equals `value`, mutably.
    pub fn child_where_mut(&mut self, name: &str, attr: &str, value: &str) -> Option<&mut Element> {
        self.children_named_mut(name)
            .find(|e| e.attr(attr) == Some(value))
    }

    pub fn push(&mut self, child: Element) -> &mut Self {
        self.children.push(Node::Element(child));
        self
    }

    /// Insert `child` directly after the first `after` element, or append it if
    /// there is none. Element order in the document is not known to matter to
    /// the device's parser, but keeping it where the vendor's client puts it
    /// costs nothing.
    pub fn insert_after(&mut self, after: &str, child: Element) {
        let at = self
            .children
            .iter()
            .position(|n| matches!(n, Node::Element(e) if e.name == after));
        match at {
            Some(i) => self.children.insert(i + 1, Node::Element(child)),
            None => {
                self.push(child);
            }
        }
    }

    /// Drop every direct child element called `name`.
    pub fn remove_children(&mut self, name: &str) {
        self.children
            .retain(|n| !matches!(n, Node::Element(e) if e.name == name));
    }

    /// The `name` child, created empty if it is not there yet.
    pub fn child_or_insert(&mut self, name: &str) -> &mut Element {
        if self.child(name).is_none() {
            self.push(Element::new(name));
        }
        self.child_mut(name).expect("just inserted")
    }

    /// Serialize back to XML.
    pub fn to_xml(&self) -> String {
        let mut writer = Writer::new(Vec::new());
        write_element(&mut writer, self);
        String::from_utf8(writer.into_inner()).expect("XML writer emits UTF-8")
    }
}

fn write_element(writer: &mut Writer<Vec<u8>>, e: &Element) {
    let mut start = BytesStart::new(e.name.as_str());
    for (k, v) in &e.attrs {
        start.push_attribute((k.as_str(), v.as_str()));
    }
    if e.children.is_empty() {
        writer.write_event(Event::Empty(start)).expect("in-memory");
        return;
    }
    writer
        .write_event(Event::Start(start.borrow()))
        .expect("in-memory");
    for child in &e.children {
        match child {
            Node::Element(c) => write_element(writer, c),
            Node::Text(t) => writer
                .write_event(Event::Text(BytesText::new(t)))
                .expect("in-memory"),
        }
    }
    writer
        .write_event(Event::End(start.to_end()))
        .expect("in-memory");
}

/// Parse `xml` and return the first element called `want`, with its subtree.
///
/// Returns `Ok(None)` when the document has no such element.
pub fn find_element(xml: &str, want: &str) -> Result<Option<Element>, Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    // Empty elements are reported as Start+End rather than Empty so that one
    // arm handles both; the writer decides the output form from the children.
    reader.config_mut().expand_empty_elements = true;

    let mut buf = Vec::new();
    // Elements currently open, once we are inside the one we want.
    let mut stack: Vec<Element> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,

            Event::Start(ref e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if stack.is_empty() && name != want {
                    continue;
                }
                stack.push(Element {
                    name,
                    attrs: attrs_of(e)?,
                    children: Vec::new(),
                });
            }

            Event::End(_) => {
                let Some(done) = stack.pop() else { continue };
                match stack.last_mut() {
                    Some(parent) => {
                        parent.push(done);
                    }
                    // The element we were asked for just closed.
                    None => return Ok(Some(done)),
                }
            }

            Event::Text(ref t) => {
                if let Some(open) = stack.last_mut() {
                    let text = t.unescape()?.into_owned();
                    if !text.trim().is_empty() {
                        open.children.push(Node::Text(text));
                    }
                }
            }

            // A backup blob and an installed certificate arrive as CDATA. It is
            // kept as text; nothing here writes CDATA back out, and the payload
            // is the same either way.
            Event::CData(ref t) => {
                if let Some(open) = stack.last_mut() {
                    let text = String::from_utf8_lossy(t.as_ref()).into_owned();
                    if !text.trim().is_empty() {
                        open.children.push(Node::Text(text));
                    }
                }
            }

            _ => {}
        }
        buf.clear();
    }

    Ok(None)
}

fn attrs_of(e: &BytesStart<'_>) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    for a in e.attributes().with_checks(false) {
        let a = a?;
        out.push((
            String::from_utf8_lossy(a.key.as_ref()).into_owned(),
            a.unescape_value()?.into_owned(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"<PV_Message name="PV_CONFIG">
        <config_data config_id="00/00/00 00:00:00 0">
        <proxyview name="kvm&amp;rack" type="108">
        <connection idle_time="10" ip_port="900"/>
        <aaa><radius realm="" accounting="false">
        <server host="10.2.11.11" port="1812" secret="s3cr3t"/>
        </radius></aaa>
        <targets><target name="One" port="1"/><target name="Two" port="2"/></targets>
        </proxyview></config_data></PV_Message>"#;

    fn doc() -> Element {
        find_element(DOC, "config_data").unwrap().unwrap()
    }

    #[test]
    fn finds_the_named_element_with_its_subtree() {
        let d = doc();
        assert_eq!(d.name, "config_data");
        assert_eq!(d.attr("config_id"), Some("00/00/00 00:00:00 0"));
        let pv = d.child("proxyview").unwrap();
        // Entities are decoded on the way in and re-encoded on the way out.
        assert_eq!(pv.attr("name"), Some("kvm&rack"));
        assert_eq!(
            pv.child("connection").unwrap().attr_parsed("ip_port"),
            Some(900u16)
        );
    }

    #[test]
    fn missing_element_is_not_an_error() {
        assert_eq!(find_element(DOC, "nonesuch").unwrap(), None);
    }

    #[test]
    fn round_trips_unknown_attributes_and_elements() {
        let d = doc();
        let xml = d.to_xml();
        // Nothing the model does not know about is lost.
        assert!(xml.contains(r#"secret="s3cr3t""#));
        assert!(xml.contains(r#"type="108""#));
        assert!(xml.contains(r#"name="kvm&amp;rack""#));
        // And it parses back to the same tree.
        assert_eq!(find_element(&xml, "config_data").unwrap().unwrap(), d);
    }

    #[test]
    fn set_keeps_attribute_position() {
        let mut d = doc();
        d.child_mut("proxyview")
            .unwrap()
            .child_mut("connection")
            .unwrap()
            .set("idle_time", "30");
        let conn = d.child("proxyview").unwrap().child("connection").unwrap();
        assert_eq!(
            conn.attrs,
            vec![
                ("idle_time".into(), "30".into()),
                ("ip_port".into(), "900".into())
            ]
        );
    }

    #[test]
    fn selects_a_child_by_attribute() {
        let mut d = doc();
        let targets = d
            .child_mut("proxyview")
            .unwrap()
            .child_mut("targets")
            .unwrap();
        targets
            .child_where_mut("target", "port", "2")
            .unwrap()
            .set("name", "Renamed");
        assert_eq!(
            targets
                .child_where("target", "port", "2")
                .unwrap()
                .attr("name"),
            Some("Renamed")
        );
        assert_eq!(
            targets
                .child_where("target", "port", "1")
                .unwrap()
                .attr("name"),
            Some("One")
        );
    }

    #[test]
    fn empty_elements_stay_empty() {
        let e = find_element(r#"<a><b x="1"/></a>"#, "a").unwrap().unwrap();
        assert_eq!(e.to_xml(), r#"<a><b x="1"/></a>"#);
    }

    #[test]
    fn insert_after_places_the_element_where_asked() {
        let mut d = doc();
        let pv = d.child_mut("proxyview").unwrap();
        pv.insert_after("connection", Element::new("SSH"));
        let order: Vec<&str> = pv
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["connection", "SSH", "aaa", "targets"]);
    }
}
