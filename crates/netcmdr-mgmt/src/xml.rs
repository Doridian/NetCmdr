// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small lossless XML tree.
//!
//! Settings are changed by posting back the whole config document
//! (`SaveCfgHttpCmd`), and anything dropped is dropped on the device. So this
//! preserves attribute order and unknown content, unlike [`crate::model`].
//!
//! No namespaces, DTDs or processing instructions; the device emits none.

use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// An element with attributes in document order.
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

    /// Trimmed, since the firmware adds stray newlines (`name="D1214403\n"`).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
    }

    /// `""` when absent.
    pub fn attr_or_empty(&self, name: &str) -> &str {
        self.attr(name).unwrap_or("")
    }

    /// `"true"`/`"1"` → true; else false.
    pub fn attr_bool(&self, name: &str) -> bool {
        matches!(self.attr(name), Some(v) if v.eq_ignore_ascii_case("true") || v == "1")
    }

    pub fn attr_parsed<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.attr(name).and_then(|v| v.parse().ok())
    }

    /// Direct text and CDATA, concatenated.
    pub fn text(&self) -> String {
        self.children
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Set an attribute, keeping its position if present.
    pub fn set(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        let value = value.into();
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => *v = value,
            None => self.attrs.push((name.to_string(), value)),
        }
        self
    }

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

    pub fn child_where_mut(&mut self, name: &str, attr: &str, value: &str) -> Option<&mut Element> {
        self.children_named_mut(name)
            .find(|e| e.attr(attr) == Some(value))
    }

    pub fn push(&mut self, child: Element) -> &mut Self {
        self.children.push(Node::Element(child));
        self
    }

    /// Insert `child` after the first `after` element, else append.
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

    pub fn remove_children(&mut self, name: &str) {
        self.children
            .retain(|n| !matches!(n, Node::Element(e) if e.name == name));
    }

    /// The `name` child, created if missing.
    pub fn child_or_insert(&mut self, name: &str) -> &mut Element {
        if self.child(name).is_none() {
            self.push(Element::new(name));
        }
        self.child_mut(name).expect("just inserted")
    }

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

/// The first `want` element and its subtree, or `Ok(None)`.
pub fn find_element(xml: &str, want: &str) -> Result<Option<Element>, Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    // Report empty elements as Start+End so one arm handles both.
    reader.config_mut().expand_empty_elements = true;

    let mut buf = Vec::new();
    // Open elements inside the wanted one.
    let mut stack: Vec<Element> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,

            Event::Start(ref e) => {
                let name = e.name().as_ref().to_owned();
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
                    None => return Ok(Some(done)),
                }
            }

            Event::Text(ref t) => {
                if let Some(open) = stack.last_mut() {
                    let text = quick_xml::escape::unescape(t)
                        .map_err(quick_xml::Error::from)?
                        .into_owned();
                    if !text.trim().is_empty() {
                        open.children.push(Node::Text(text));
                    }
                }
            }

            // Backups and certificates arrive as CDATA; kept as text.
            Event::CData(ref t) => {
                if let Some(open) = stack.last_mut() {
                    let text = (**t).to_owned();
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
            a.key.as_ref().to_owned(),
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                .into_owned(),
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
        assert!(xml.contains(r#"secret="s3cr3t""#));
        assert!(xml.contains(r#"type="108""#));
        assert!(xml.contains(r#"name="kvm&amp;rack""#));
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
