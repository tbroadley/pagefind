use hashbrown::HashMap;
use lazy_static::lazy_static;
use lol_html::html_content::Element;
use lol_html::{element, text, HtmlRewriter, Settings};
use regex::Regex;
use std::cell::RefCell;
use std::default::Default;
use std::rc::Rc;

use crate::SearchOptions;

lazy_static! {
    static ref NEWLINES: Regex = Regex::new("(\n|\r\n)+").unwrap();
    static ref TRIM_NEWLINES: Regex = Regex::new("^[\n\r\\s]+|[\n\r\\s]+$").unwrap();
    static ref EXTRANEOUS_SPACES: Regex = Regex::new("\\s{2,}").unwrap();
    static ref SENTENCE_CHARS: Regex = Regex::new("[\\w'\"\\)\\$\\*]").unwrap();
}
lazy_static! {
    static ref ATTRIBUTE_MATCH: Regex =
        Regex::new("^\\s*(?P<name>[^:\\[\\]]+)\\[(?P<attribute>.+)\\]\\s*$").unwrap();
}
lazy_static! {
    static ref SENTENCE_SELECTORS: Vec<&'static str> = vec!(
        "h1", "h2", "h3", "h4", "h5", "h6", "p", "td", "div", "ul", "li", "article", "section"
    );
    static ref REMOVE_SELECTORS: Vec<&'static str> = vec!(
        "head", "script", "noscript", "label", "form", "svg", "footer", "header", "nav", "iframe",
        "template"
    );
}

// We aren't transforming HTML, just parsing, so we dump the output.
#[derive(Default)]
struct EmptySink;
impl lol_html::OutputSink for EmptySink {
    fn handle_chunk(&mut self, _: &[u8]) {}
}

/// Houses the HTML parsing instance and the internal data while parsing
pub struct DomParser<'a> {
    rewriter: HtmlRewriter<'a, EmptySink>,
    data: Rc<RefCell<DomParserData>>,
}

// The internal state while parsing,
// with a reference to the deepest HTML element
// that we're currently reading
#[derive(Default, Debug)]
struct DomParserData {
    current_node: Rc<RefCell<DomParsingNode>>,
    filters: HashMap<String, Vec<String>>,
    meta: HashMap<String, String>,
}

#[derive(Debug, PartialEq)]
enum NodeStatus {
    Indexing,
    Ignored,
    Body,
    // There was a body element below us,
    // so our content should be ignored.
    ParentOfBody,
}

impl Default for NodeStatus {
    fn default() -> Self {
        Self::Indexing
    }
}

// A single HTML element that we're reading into.
// Contains a reference to the parent element,
// and since we collapse this tree upwards while we parse,
// we don't need to store tree structure.
#[derive(Default, Debug)]
struct DomParsingNode {
    current_value: String,
    parent: Option<Rc<RefCell<DomParsingNode>>>,
    filter: Option<Vec<String>>,
    meta: Option<Vec<String>>,
    status: NodeStatus,
}

/// The search-relevant data that was retrieved from the given input
#[derive(Debug)]
pub struct DomParserResult {
    pub digest: String,
    pub filters: HashMap<String, Vec<String>>,
    pub meta: HashMap<String, String>,
    pub has_custom_body: bool,
}

// Some shorthand to clean up our use of Rc<RefCell<*>> in the lol_html macros
// From https://github.com/rust-lang/rfcs/issues/2407#issuecomment-385291238
macro_rules! enclose {
    ( ($( $x:ident ),*) $y:expr ) => {
        {
            $(let $x = $x.clone();)*
            $y
        }
    };
}

impl<'a> DomParser<'a> {
    pub fn new(options: &SearchOptions) -> Self {
        let data = Rc::new(RefCell::new(DomParserData::default()));
        let root = format!("{}, {} *", options.root_selector, options.root_selector);

        let rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    enclose! { (data) element!(root, move |el| {
                        let should_ignore_el = el.has_attribute("data-pagefind-ignore") || REMOVE_SELECTORS.contains(&el.tag_name().as_str());
                        let treat_as_body = el.has_attribute("data-pagefind-body");
                        let filter = el.get_attribute("data-pagefind-filter").map(|attr| parse_attr_string(attr, el));
                        let meta = el.get_attribute("data-pagefind-meta").map(|attr| parse_attr_string(attr, el));
                        let index_attrs: Option<Vec<String>> = el.get_attribute("data-pagefind-index-attrs").map(|attr| attr.split(',').map(|a| a.trim().to_string()).collect());
                        let tag_name = el.tag_name();

                        let status = if treat_as_body {
                            NodeStatus::Body
                        } else if should_ignore_el {
                            NodeStatus::Ignored
                        } else {
                            NodeStatus::Indexing
                        };

                        let node = Rc::new(RefCell::new(DomParsingNode{
                            parent: Some(Rc::clone(&data.borrow().current_node)),
                            status,
                            filter,
                            meta,
                            ..DomParsingNode::default()
                        }));

                        if let Some(attrs) = index_attrs {
                            let parent = &data.borrow().current_node;
                            for attr in attrs {
                                let mut value = el.get_attribute(attr.trim()).unwrap_or_default();
                                if value.chars()
                                    .last()
                                    .filter(|c| SENTENCE_CHARS.is_match(&c.to_string()))
                                    .is_some() {
                                        value.push('.');
                                    }
                                parent.borrow_mut().current_value.push(' ');
                                parent.borrow_mut().current_value.push_str(&value);
                                parent.borrow_mut().current_value.push(' ');
                            }
                        }

                        {
                            let mut data = data.borrow_mut();
                            data.current_node = Rc::clone(&node);
                        }

                        let can_have_content = el.on_end_tag(enclose! { (data, node, tag_name) move |end| {
                            let mut data = data.borrow_mut();
                            let mut node = node.borrow_mut();

                            // When we reach an end tag, we need to
                            // make sure to move focus back to the parent node.
                            if let Some(parent) = &node.parent {
                                data.current_node = Rc::clone(parent);
                            }

                            // Process filters & meta before we continue
                            // (Filters & meta are valid on ignored elements)
                            if let Some(filters) = &node.filter {
                                for filter in filters {
                                    if let Some((filter, value)) = node.get_attribute_pair(filter) {
                                        match data.filters.get_mut(&filter) {
                                            Some(filter_arr) => filter_arr.push(normalize_content(&value)),
                                            None => {
                                                data.filters.insert(filter, vec![
                                                    normalize_content(&value)
                                                ]);
                                            }
                                        }
                                    }
                                }
                            }

                            if let Some(metas) = &node.meta {
                                for meta in metas {
                                    if let Some((meta, value)) = node.get_attribute_pair(meta) {
                                        data.meta.insert(meta, value);
                                    }
                                }
                            }

                            // Try to capture the first title on the page (if unset)
                            if tag_name == "h1" && !data.meta.contains_key("auto_title") {
                                data.meta.insert("auto_title".into(), normalize_content(&node.current_value));
                            }

                            // If we bail out now, the content won't be persisted anywhere
                            // and the node + children will be dropped.
                            if node.status == NodeStatus::Ignored {
                                return Ok(());
                            }

                            let tag_name = end.name();
                            if SENTENCE_SELECTORS.contains(&tag_name.as_str()) {
                                // For block elements, we want to make sure sentences
                                // don't hug each other without whitespace.
                                // We normalize repeated whitespace later, so we
                                // can add this indiscriminately.
                                let mut padded = " ".to_owned();
                                padded.push_str(&node.current_value);
                                node.current_value = padded;

                                // Similarly, we want to separate block elements
                                // with punctuation, so that the excerpts read nicely.
                                // (As long as it doesn't already end with, say, a . or ?)
                                if node.current_value.chars()
                                    .last()
                                    .filter(|c| SENTENCE_CHARS.is_match(&c.to_string()))
                                    .is_some() {
                                        node.current_value.push('.');
                                }
                                node.current_value.push(' ');
                            }

                            // Huck all of the content we have onto the end of the
                            // content that the parent node has (so far)
                            // This will include all of our children's content,
                            // and the order of tree traversal will mean that it
                            // is inserted in the correct position in the parent's content.
                            let mut parent = data.current_node.borrow_mut();

                            // If the parent is a parent of a body, we don't want to append
                            // any more content to it. (Unless, of course, we are another body)
                            if node.status != NodeStatus::Body && parent.status == NodeStatus::ParentOfBody {
                                return Ok(());
                            }
                            match node.status {
                                NodeStatus::Ignored => {},
                                NodeStatus::Indexing => {
                                    parent.current_value.push_str(&node.current_value);
                                },
                                NodeStatus::Body | NodeStatus::ParentOfBody => {
                                    // If our parent is already a parent of a body, then
                                    // we're probably a subsequent body. Avoid clearing it out.
                                    if parent.status != NodeStatus::ParentOfBody {
                                        parent.current_value.clear();
                                    }
                                    parent.current_value.push_str(&node.current_value);
                                    parent.status = NodeStatus::ParentOfBody;
                                }
                            };

                            Ok(())
                        }});

                        // Try to handle tags like <img /> which have no end tag,
                        // and thus will never hit the logic to reset the current node.
                        // TODO: This could still be missed for tags with implied ends?
                        if can_have_content.is_err() {
                            let mut data = data.borrow_mut();
                            let node = node.borrow();
                            if let Some(parent) = &node.parent {
                                data.current_node = Rc::clone(parent);
                            }

                            // Process filters & meta before we continue
                            // TODO: Abstract repitition into function
                            if let Some(filters) = &node.filter {
                                for filter in filters {
                                    if let Some((filter, value)) = node.get_attribute_pair(filter) {
                                        match data.filters.get_mut(&filter) {
                                            Some(filter_arr) => filter_arr.push(normalize_content(&value)),
                                            None => {
                                                data.filters.insert(filter, vec![
                                                    normalize_content(&value)
                                                ]);
                                            }
                                        }
                                    }
                                }
                            }

                            if let Some(metas) = &node.meta {
                                for meta in metas {
                                    if let Some((meta, value)) = node.get_attribute_pair(meta) {
                                        data.meta.insert(meta, value);
                                    }
                                }
                            }

                            // Try to capture the first image _after_ a title (if unset)
                            if tag_name == "img"
                                && !data.meta.contains_key("auto_image")
                                && (data.meta.contains_key("auto_title") || data.meta.contains_key("title")) {
                                if let Some(src) = el.get_attribute("src") {
                                    data.meta.insert("auto_image".into(), src);

                                    if let Some(alt) = el.get_attribute("alt") {
                                        data.meta.insert("auto_image_alt".into(), alt);
                                    }
                                }
                            }
                        }
                        Ok(())
                    })},
                    // Slap any text we encounter inside the body into the current node's current value
                    enclose! { (data) text!(&options.root_selector, move |el| {
                        let data = data.borrow_mut();
                        let mut node = data.current_node.borrow_mut();
                        node.current_value.push_str(el.as_str());
                        Ok(())
                    })},
                ],
                ..Settings::default()
            },
            EmptySink::default(),
        );

        Self { rewriter, data }
    }

    /// Writes a chunk of data to the underlying HTML parser
    pub fn write(&mut self, data: &[u8]) -> Result<(), lol_html::errors::RewritingError> {
        self.rewriter.write(data)
    }

    /// Performs any post-processing and returns the summated search results
    pub fn wrap(self) -> DomParserResult {
        drop(self.rewriter); // Clears the extra Rcs on and within data
        let mut data = Rc::try_unwrap(self.data).unwrap().into_inner();
        let mut node = data.current_node;

        // Fallback: If we are left with a tree, collapse it up into the parents
        // until we get to the root node.
        while node.borrow().parent.is_some() {
            {
                let node = node.borrow();
                let mut parent = node.parent.as_ref().unwrap().borrow_mut();
                if parent.status != NodeStatus::ParentOfBody {
                    match node.status {
                        NodeStatus::Ignored => {}
                        NodeStatus::Indexing => {
                            parent.current_value.push_str(&node.current_value);
                        }
                        NodeStatus::Body | NodeStatus::ParentOfBody => {
                            parent.current_value.clear();
                            parent.current_value.push_str(&node.current_value);
                            parent.status = NodeStatus::ParentOfBody;
                        }
                    };
                }
            }
            let old_node = node.borrow();
            let new_node = Rc::clone(old_node.parent.as_ref().unwrap());
            drop(old_node);
            node = new_node;
        }

        if let Some(image) = data.meta.remove("auto_image") {
            let alt = data.meta.remove("auto_image_alt").unwrap_or_default();
            if !data.meta.contains_key("image") {
                data.meta.insert("image".into(), image);
                data.meta.insert("image_alt".into(), alt);
            }
        }

        if let Some(title) = data.meta.remove("auto_title") {
            if !data.meta.contains_key("title") {
                data.meta.insert("title".into(), title);
            }
        }

        let node = node.borrow();
        DomParserResult {
            digest: normalize_content(&node.current_value),
            filters: data.filters,
            meta: data.meta,
            has_custom_body: node.status == NodeStatus::ParentOfBody,
        }
    }
}

fn normalize_content(content: &str) -> String {
    let content = TRIM_NEWLINES.replace_all(content, "");
    let content = NEWLINES.replace_all(&content, " ");
    let content = EXTRANEOUS_SPACES.replace_all(&content, " ");

    content.to_string()
}

fn parse_attr_string(input: String, el: &Element) -> Vec<String> {
    if let Some((attrs, literal)) = input.split_once(':') {
        let mut attrs = parse_attr_string(attrs.to_owned(), el);
        if let Some(last) = attrs.last_mut() {
            last.push(':');
            last.push_str(literal);
        }
        return attrs;
    }
    input
        .split(',')
        .map(|chunk| {
            let chunk = chunk.trim();
            if let Some(value) = ATTRIBUTE_MATCH.captures(chunk) {
                let name = value.name("name").unwrap().as_str().to_owned();
                let attr = value.name("attribute").unwrap().as_str().to_owned();
                format!("{}:{}", name, el.get_attribute(&attr).unwrap_or_default())
            } else {
                chunk.to_owned()
            }
        })
        .collect()
}

impl DomParsingNode {
    fn get_attribute_pair(&self, input: &str) -> Option<(String, String)> {
        match input.split_once(":") {
            Some((filter, value)) => Some((filter.to_owned(), value.to_owned())),
            None => {
                if self.current_value.is_empty() {
                    None
                } else {
                    Some((input.to_owned(), self.current_value.to_owned()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_content() {
        let input = "\nHello  Wor\n ld? \n \n";
        let output = normalize_content(input);

        assert_eq!(&output, "Hello Wor ld?");
    }

    #[test]
    fn get_filter_from_node() {
        let mut node = DomParsingNode::default();

        assert_eq!(node.get_attribute_pair("color"), None);

        node.current_value = "White".into();
        assert_eq!(
            node.get_attribute_pair("color"),
            Some(("color".into(), "White".into()))
        );

        assert_eq!(
            node.get_attribute_pair("color:auburn"),
            Some(("color".into(), "auburn".into()))
        );

        assert_eq!(
            node.get_attribute_pair("color:ye:llow"),
            Some(("color".into(), "ye:llow".into()))
        );
    }

    fn test_raw_parse(input: Vec<&'static str>) -> DomParserResult {
        let config_args = vec![twelf::Layer::Clap(
            <crate::PagefindInboundConfig as clap::IntoApp>::command().get_matches_from(vec![
                "pagefind",
                "--source",
                "not_important",
            ]),
        )];
        let config =
            SearchOptions::load(crate::PagefindInboundConfig::with_layers(&config_args).unwrap())
                .unwrap();
        let mut rewriter = DomParser::new(&config);
        for line in input {
            let _ = rewriter.write(line.as_bytes());
        }
        rewriter.wrap()
    }

    fn test_parse(mut input: Vec<&'static str>) -> DomParserResult {
        input.insert(0, "<html><body>");
        input.push("</body></html>");
        test_raw_parse(input)
    }

    #[test]
    fn block_tag_formatting() {
        let data = test_parse(vec![
            "<p>Sentences should have periods</p>",
            "<p>Unless one exists.</p>",
            "<div>Or it ends with punctuation:</div>",
            "<article>Except for 'quotes'</article>",
        ]);

        assert_eq!(
            data.digest,
            "Sentences should have periods. Unless one exists. Or it ends with punctuation: Except for 'quotes'."
        )
    }

    #[test]
    fn inline_tag_formatting() {
        let data = test_parse(vec![
            "<p>Inline tags like <span>span</span>",
            " and <b>bol",
            "d</b> shouldn't have periods</p>",
            "<p>And should n<i>o</i>t add any space.</p>",
        ]);

        assert_eq!(
            data.digest,
            "Inline tags like span and bold shouldn't have periods. And should not add any space."
        )
    }

    #[test]
    fn ignored_elements() {
        let data = test_parse(vec![
            "<p>Elements like:</p>",
            "<form>Should <b>not</b> be indexed</form>",
            "<p>forms</p>",
            "<div> As well as <div data-pagefind-ignore=''>",
            "Manually ignored <p>Elements</p></div>",
            "*crickets*</div>",
        ]);

        assert_eq!(data.digest, "Elements like: forms. As well as *crickets*.");
    }

    #[test]
    fn return_metadata() {
        let data = test_raw_parse(vec![
            "<html><head>",
            "<meta data-pagefind-meta='image[content]' content='/kitty.jpg' property='og:image'>",
            "</head><body>",
            "<div data-pagefind-meta='type:post'></div>",
            "<h1 data-pagefind-meta='headline'>Hello World</h1>",
            "<div>This post is <span data-pagefind-meta='adj'>hella</span> good.</div>",
            "<img data-pagefind-meta='hero[src]' src='/huzzah.png'>",
            "</body></html>",
        ]);

        assert_eq!(data.meta.get("image"), Some(&"/kitty.jpg".to_owned()));
        assert_eq!(data.meta.get("type"), Some(&"post".to_owned()));
        assert_eq!(data.meta.get("headline"), Some(&"Hello World".to_owned()));
        assert_eq!(data.meta.get("adj"), Some(&"hella".to_owned()));
        assert_eq!(data.meta.get("hero"), Some(&"/huzzah.png".to_owned()));
    }

    #[test]
    fn return_complex_metadata() {
        let data = test_raw_parse(vec![
            "<html><body>",
            "<img data-pagefind-meta='cat[src], cat-alt[alt]' src='/cat.png' alt='cat pic'>",
            "<h1 class='why?' data-pagefind-meta='headline, classname[class]'>Hello World</h1>",
            "<div data-pagefind-meta='self[data-pagefind-meta], type:post'></div>",
            "<div data-pagefind-meta='incorrect:post, self[data-pagefind-meta]'></div>",
            "</body></html>",
        ]);

        assert_eq!(data.meta.get("cat"), Some(&"/cat.png".to_owned()));
        assert_eq!(data.meta.get("cat-alt"), Some(&"cat pic".to_owned()));
        assert_eq!(data.meta.get("headline"), Some(&"Hello World".to_owned()));
        assert_eq!(data.meta.get("classname"), Some(&"why?".to_owned()));
        assert_eq!(
            data.meta.get("self"),
            Some(&"self[data-pagefind-meta], type:post".to_owned())
        );
        assert_eq!(data.meta.get("type"), Some(&"post".to_owned()));
        assert_eq!(
            data.meta.get("incorrect"),
            Some(&"post, self[data-pagefind-meta]".to_owned())
        );
    }
}
