//! The UI hierarchy `uiautomator dump` writes: its nodes, and how a target is
//! resolved from them.
//!
//! The dump is machine-generated XML: one `<hierarchy>` element holding nested
//! `<node>` elements whose attributes carry the text, the resource id, the
//! content description, the class and the screen rectangle of each element. The
//! hierarchy itself is not needed to act on an element, only the nodes and their
//! rectangles, so the parser reads the attributes of every `<node>` tag and
//! keeps the nodes in document order.
//!
//! Nothing here asks the device for anything: the two seconds a dump costs are
//! spent by the step that decides it needs the tree.

/// The screen rectangle one node occupies, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) left: i64,
    pub(crate) top: i64,
    pub(crate) right: i64,
    pub(crate) bottom: i64,
}

impl Bounds {
    /// The point a tap must hit to land on this node.
    pub(crate) fn center(self) -> (i64, i64) {
        ((self.left + self.right) / 2, (self.top + self.bottom) / 2)
    }
}

/// One node of the hierarchy, as the dump describes it.
///
/// Every field is optional because a vendor build may leave one out; a missing
/// attribute stays `None` instead of becoming an empty string, so a case can
/// assert on `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) text: Option<String>,
    pub(crate) resource_id: Option<String>,
    pub(crate) content_desc: Option<String>,
    pub(crate) class: Option<String>,
    pub(crate) bounds: Option<Bounds>,
}

/// How a case names one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Filter {
    /// The node whose text is exactly this value, ignoring surrounding whitespace.
    Text(String),
    /// The node whose resource id is exactly this value, for example
    /// `com.example.app:id/ok`.
    ResourceId(String),
}

impl Filter {
    /// The filter as a sentence fragment, for an error message.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Text(text) => format!("with text `{}`", text.trim()),
            Self::ResourceId(resource_id) => format!("with resource id `{resource_id}`"),
        }
    }

    fn matches(&self, node: &Node) -> bool {
        match self {
            Self::Text(text) => node
                .text
                .as_deref()
                .is_some_and(|value| value.trim() == text.trim()),
            Self::ResourceId(resource_id) => node.resource_id.as_deref() == Some(resource_id),
        }
    }
}

/// Why a filter did not name exactly one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// No node matched.
    NotFound,
    /// Several nodes matched; their indexes in document order.
    Ambiguous(Vec<usize>),
}

/// Finds the one node the filter names.
///
/// A filter that matches two nodes is an error rather than the first match:
/// silently tapping the first of two `OK` buttons is how a case passes while
/// testing the wrong screen.
pub(crate) fn resolve<'a>(nodes: &'a [Node], filter: &Filter) -> Result<&'a Node, ResolveError> {
    let mut matches = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| filter.matches(node))
        .map(|(index, _)| index);
    let Some(first) = matches.next() else {
        return Err(ResolveError::NotFound);
    };
    let rest: Vec<usize> = matches.collect();
    if rest.is_empty() {
        return Ok(&nodes[first]);
    }
    let mut all = vec![first];
    all.extend(rest);
    Err(ResolveError::Ambiguous(all))
}

/// Parses every `<node>` of a dump, in document order.
///
/// The error is a message for the case author, not a position: a dump that
/// cannot be read means the device answered with something other than a
/// hierarchy.
pub(crate) fn parse(xml: &str) -> Result<Vec<Node>, String> {
    if !xml.contains("<hierarchy") {
        return Err(format!(
            "the device did not return a uiautomator hierarchy ({} bytes): {}",
            xml.len(),
            first_line(xml)
        ));
    }
    let mut nodes = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<node") {
        let after = &rest[start + "<node".len()..];
        let (attributes, used) = read_tag(after)?;
        let mut node = Node::default();
        for (name, value) in attributes {
            match name.as_str() {
                "text" => node.text = non_empty(value),
                "resource-id" => node.resource_id = non_empty(value),
                "content-desc" => node.content_desc = non_empty(value),
                "class" => node.class = non_empty(value),
                "bounds" => node.bounds = parse_bounds(&value),
                _ => {}
            }
        }
        nodes.push(node);
        rest = &after[used..];
    }
    Ok(nodes)
}

/// Reads the attributes of one tag and how many bytes the tag occupies.
///
/// The dump always quotes a value with `"`, so a `>` inside a value cannot end
/// the tag; the scanner follows the quoting instead of searching for `>`.
fn read_tag(input: &str) -> Result<(Vec<(String, String)>, usize), String> {
    let bytes = input.as_bytes();
    let mut attributes = Vec::new();
    let mut index = 0;
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        match bytes.get(index) {
            Some(b'>') => return Ok((attributes, index + 1)),
            Some(b'/') if bytes.get(index + 1) == Some(&b'>') => {
                return Ok((attributes, index + 2));
            }
            Some(_) => {}
            None => return Err("the dump ends inside a `<node>` tag".to_string()),
        }

        let name_start = index;
        while index < bytes.len()
            && !matches!(
                bytes[index],
                b'=' | b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            index += 1;
        }
        let name = input[name_start..index].to_string();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            return Err(format!(
                "the dump has an attribute `{name}` without a value"
            ));
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'"') {
            return Err(format!(
                "the dump has an attribute `{name}` whose value is not double-quoted"
            ));
        }
        index += 1;
        let value_start = index;
        while index < bytes.len() && bytes[index] != b'"' {
            index += 1;
        }
        if index >= bytes.len() {
            return Err(format!("the dump ends inside the value of `{name}`"));
        }
        attributes.push((name, decode_entities(&input[value_start..index])));
        index += 1;
    }
}

/// Reads the `[left,top][right,bottom]` form the dump uses for a rectangle.
fn parse_bounds(value: &str) -> Option<Bounds> {
    let inner = value.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (first, second) = inner.split_once("][")?;
    let mut first = first.split(',');
    let mut second = second.split(',');
    let left = first.next()?.trim().parse().ok()?;
    let top = first.next()?.trim().parse().ok()?;
    let right = second.next()?.trim().parse().ok()?;
    let bottom = second.next()?.trim().parse().ok()?;
    Some(Bounds {
        left,
        top,
        right,
        bottom,
    })
}

/// An attribute value that carries no content is absent.
fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

/// Decodes the five XML entities and the numeric ones the dump can contain.
fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            other => decode_numeric_entity(other),
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &tail[end + 1..];
            }
            // An unknown entity stays as written instead of being dropped.
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decodes `#123` or `#x1f600`.
fn decode_numeric_entity(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let code = match digits
        .strip_prefix('x')
        .or_else(|| digits.strip_prefix('X'))
    {
        Some(hexadecimal) => u32::from_str_radix(hexadecimal, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

/// The first line of a device answer, for the message of an unreadable dump.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("the answer was empty")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hierarchy shaped the way `uiautomator dump` writes one.
    const DUMP: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.android.settings" content-desc="" checkable="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][720,1280]"><node index="0" text="Search settings" resource-id="com.android.settings:id/search_bar" class="android.widget.TextView" package="com.android.settings" content-desc="Search" checkable="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[24,120][696,200]" /><node index="1" text="Network &amp; internet" resource-id="com.android.settings:id/homepage_title" class="android.widget.TextView" package="com.android.settings" content-desc="" checkable="false" clickable="false" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[24,240][696,320]" /></node></hierarchy>"#;

    #[test]
    fn every_node_is_read_in_document_order_with_its_values() {
        let nodes = parse(DUMP).expect("the sample dump must parse");

        assert_eq!(nodes.len(), 3);
        assert_eq!(
            nodes[0].class.as_deref(),
            Some("android.widget.FrameLayout")
        );
        assert_eq!(nodes[1].text.as_deref(), Some("Search settings"));
        assert_eq!(
            nodes[1].resource_id.as_deref(),
            Some("com.android.settings:id/search_bar")
        );
        assert_eq!(nodes[1].content_desc.as_deref(), Some("Search"));
        assert_eq!(
            nodes[1].bounds,
            Some(Bounds {
                left: 24,
                top: 120,
                right: 696,
                bottom: 200,
            })
        );
        assert_eq!(nodes[0].text, None, "an empty attribute is absent");
        assert_eq!(nodes[2].text.as_deref(), Some("Network & internet"));
    }

    #[test]
    fn a_leaf_node_that_closes_itself_is_read_too() {
        let nodes = parse(DUMP).expect("the sample dump must parse");
        assert!(
            nodes[1].resource_id.as_deref() == Some("com.android.settings:id/search_bar"),
            "a self-closing node must be kept: {nodes:?}"
        );
    }

    #[test]
    fn the_rectangle_gives_the_point_a_tap_must_hit() {
        let bounds = Bounds {
            left: 24,
            top: 120,
            right: 696,
            bottom: 201,
        };
        assert_eq!(bounds.center(), (360, 160));
    }

    #[test]
    fn an_unreadable_rectangle_leaves_the_node_without_one() {
        let nodes = parse(
            r#"<hierarchy rotation="0"><node index="0" text="OK" bounds="not a rectangle" /></hierarchy>"#,
        )
        .expect("the node must still be listed");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].text.as_deref(), Some("OK"));
        assert_eq!(nodes[0].bounds, None);
    }

    #[test]
    fn a_value_may_contain_a_greater_than_sign_or_a_quote() {
        let nodes = parse(
            r#"<hierarchy rotation="0"><node index="0" text="a &gt; b" content-desc="say &quot;hi&quot;" bounds="[0,0][10,10]" /></hierarchy>"#,
        )
        .expect("the dump must parse");
        assert_eq!(nodes[0].text.as_deref(), Some("a > b"));
        assert_eq!(nodes[0].content_desc.as_deref(), Some("say \"hi\""));
    }

    #[test]
    fn the_entities_the_dump_escapes_are_decoded() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;node&gt;"), "<node>");
        assert_eq!(decode_entities("it&apos;s"), "it's");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("50% &unknown; 100"), "50% &unknown; 100");
        assert_eq!(decode_entities("no entities here"), "no entities here");
    }

    #[test]
    fn a_text_filter_names_exactly_one_node() {
        let nodes = parse(DUMP).expect("the sample dump must parse");

        let found = resolve(&nodes, &Filter::Text("Network & internet".to_string()))
            .expect("the text is in the dump");
        assert_eq!(
            found.resource_id.as_deref(),
            Some("com.android.settings:id/homepage_title")
        );

        let padded = resolve(&nodes, &Filter::Text("  Search settings  ".to_string()))
            .expect("surrounding whitespace is ignored");
        assert_eq!(padded.class.as_deref(), Some("android.widget.TextView"));
    }

    #[test]
    fn a_resource_id_filter_names_exactly_one_node() {
        let nodes = parse(DUMP).expect("the sample dump must parse");
        let found = resolve(
            &nodes,
            &Filter::ResourceId("com.android.settings:id/search_bar".to_string()),
        )
        .expect("the resource id is in the dump");
        assert_eq!(found.text.as_deref(), Some("Search settings"));
    }

    #[test]
    fn a_filter_that_names_nothing_reports_that_nothing_matched() {
        let nodes = parse(DUMP).expect("the sample dump must parse");
        assert_eq!(
            resolve(&nodes, &Filter::Text("No such label".to_string())),
            Err(ResolveError::NotFound)
        );
        assert_eq!(
            resolve(&nodes, &Filter::ResourceId("app:id/missing".to_string())),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn a_filter_that_names_two_nodes_reports_both() {
        let nodes = parse(
            r#"<hierarchy rotation="0"><node index="0" text="OK" bounds="[0,0][10,10]" /><node index="1" text="OK" bounds="[20,20][30,30]" /></hierarchy>"#,
        )
        .expect("the dump must parse");

        assert_eq!(
            resolve(&nodes, &Filter::Text("OK".to_string())),
            Err(ResolveError::Ambiguous(vec![0, 1]))
        );
    }

    #[test]
    fn an_answer_that_is_not_a_hierarchy_is_refused_with_the_first_line() {
        let error = parse("ERROR: could not get idle state.\n").expect_err("must be refused");
        assert!(
            error.contains("did not return a uiautomator hierarchy"),
            "got: {error}"
        );
        assert!(error.contains("could not get idle state"), "got: {error}");

        let error = parse("").expect_err("must be refused");
        assert!(error.contains("the answer was empty"), "got: {error}");
    }

    #[test]
    fn an_unfinished_tag_is_refused_instead_of_misread() {
        let error =
            parse(r#"<hierarchy rotation="0"><node index="0" text="OK" bounds="[0,0][10,10]""#)
                .expect_err("an unterminated tag must be refused");
        assert!(error.contains("ends inside"), "got: {error}");
    }

    #[test]
    fn the_filter_reads_how_it_names_a_node() {
        assert_eq!(
            Filter::Text(" OK ".to_string()).describe(),
            "with text `OK`"
        );
        assert_eq!(
            Filter::ResourceId("app:id/ok".to_string()).describe(),
            "with resource id `app:id/ok`"
        );
    }
}
