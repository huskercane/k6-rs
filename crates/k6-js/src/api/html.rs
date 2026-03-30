use anyhow::Result;
use rquickjs::{Ctx, Function};
use scraper::{Html, Selector};

/// Register k6/html module.
///
/// Provides: parseHTML(html) returning a Selection object with jQuery-like methods:
/// find(selector), text(), html(), attr(name), first(), last(), size(),
/// each(callback), eq(index), children(), parent(), contents().
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // __html_parse(html) -> JSON representation of the document for JS to work with
    globals.set(
        "__html_parse",
        Function::new(ctx.clone(), |html_str: String| -> String {
            let doc = Html::parse_document(&html_str);
            // Return the raw HTML back — we'll query on-demand
            let _ = doc;
            html_str
        })?,
    )?;

    // __html_find(html, selector) -> JSON array of {html, text, attrs}
    globals.set(
        "__html_find",
        Function::new(ctx.clone(), |html_str: String, selector: String| -> String {
            let doc = Html::parse_document(&html_str);
            let sel = match Selector::parse(&selector) {
                Ok(s) => s,
                Err(_) => return "[]".to_string(),
            };

            let results: Vec<String> = doc
                .select(&sel)
                .map(|el| {
                    let inner_html = el.inner_html();
                    let text: String = el.text().collect();
                    let outer_html = el.html();

                    // Collect attributes
                    let attrs: Vec<String> = el
                        .value()
                        .attrs()
                        .map(|(k, v)| format!(r#""{}":{}"#, k, serde_json::json!(v)))
                        .collect();

                    format!(
                        r#"{{"html":{},"text":{},"outerHtml":{},"attrs":{{{}}}}}"#,
                        serde_json::json!(inner_html),
                        serde_json::json!(text),
                        serde_json::json!(outer_html),
                        attrs.join(",")
                    )
                })
                .collect();

            format!("[{}]", results.join(","))
        })?,
    )?;

    // __html_find_in_fragment(html_fragment, selector) -> same as above but for fragments
    globals.set(
        "__html_find_in_fragment",
        Function::new(ctx.clone(), |html_str: String, selector: String| -> String {
            let doc = Html::parse_fragment(&html_str);
            let sel = match Selector::parse(&selector) {
                Ok(s) => s,
                Err(_) => return "[]".to_string(),
            };

            let results: Vec<String> = doc
                .select(&sel)
                .map(|el| {
                    let inner_html = el.inner_html();
                    let text: String = el.text().collect();
                    let outer_html = el.html();

                    let attrs: Vec<String> = el
                        .value()
                        .attrs()
                        .map(|(k, v)| format!(r#""{}":{}"#, k, serde_json::json!(v)))
                        .collect();

                    format!(
                        r#"{{"html":{},"text":{},"outerHtml":{},"attrs":{{{}}}}}"#,
                        serde_json::json!(inner_html),
                        serde_json::json!(text),
                        serde_json::json!(outer_html),
                        attrs.join(",")
                    )
                })
                .collect();

            format!("[{}]", results.join(","))
        })?,
    )?;

    // __html_root_elements(html) -> JSON array of root body children
    globals.set(
        "__html_root_elements",
        Function::new(ctx.clone(), |html_str: String| -> String {
            // Return the source HTML as a single element for the Selection to work with
            let doc = Html::parse_document(&html_str);
            let root = doc.root_element();
            let outer_html = root.html();
            let inner_html = root.inner_html();
            let text: String = root.text().collect();

            let attrs: Vec<String> = root
                .value()
                .attrs()
                .map(|(k, v)| format!(r#""{}":{}"#, k, serde_json::json!(v)))
                .collect();

            format!(
                r#"[{{"html":{},"text":{},"outerHtml":{},"attrs":{{{}}}}}]"#,
                serde_json::json!(inner_html),
                serde_json::json!(text),
                serde_json::json!(outer_html),
                attrs.join(",")
            )
        })?,
    )?;

    // JS Selection API
    ctx.eval::<(), _>(
        r#"
        function Selection(elements, sourceHtml) {
            this._elements = elements || [];
            this._sourceHtml = sourceHtml || '';
        }

        Selection.prototype.find = function(selector) {
            var results = [];
            for (var i = 0; i < this._elements.length; i++) {
                var found = JSON.parse(__html_find_in_fragment(this._elements[i].outerHtml, selector));
                for (var j = 0; j < found.length; j++) {
                    results.push(found[j]);
                }
            }
            return new Selection(results, this._sourceHtml);
        };

        Selection.prototype.text = function() {
            var parts = [];
            for (var i = 0; i < this._elements.length; i++) {
                parts.push(this._elements[i].text);
            }
            return parts.join('');
        };

        Selection.prototype.html = function() {
            if (this._elements.length === 0) return '';
            return this._elements[0].html;
        };

        Selection.prototype.attr = function(name) {
            if (this._elements.length === 0) return undefined;
            return this._elements[0].attrs[name];
        };

        Selection.prototype.first = function() {
            return new Selection(this._elements.slice(0, 1), this._sourceHtml);
        };

        Selection.prototype.last = function() {
            var len = this._elements.length;
            return new Selection(len > 0 ? [this._elements[len - 1]] : [], this._sourceHtml);
        };

        Selection.prototype.eq = function(index) {
            if (index < 0) index = this._elements.length + index;
            if (index >= 0 && index < this._elements.length) {
                return new Selection([this._elements[index]], this._sourceHtml);
            }
            return new Selection([], this._sourceHtml);
        };

        Selection.prototype.size = function() {
            return this._elements.length;
        };

        Selection.prototype.each = function(callback) {
            for (var i = 0; i < this._elements.length; i++) {
                callback(i, new Selection([this._elements[i]], this._sourceHtml));
            }
        };

        Selection.prototype.map = function(callback) {
            var results = [];
            for (var i = 0; i < this._elements.length; i++) {
                results.push(callback(i, new Selection([this._elements[i]], this._sourceHtml)));
            }
            return results;
        };

        Selection.prototype.toArray = function() {
            var results = [];
            for (var i = 0; i < this._elements.length; i++) {
                results.push(new Selection([this._elements[i]], this._sourceHtml));
            }
            return results;
        };

        Selection.prototype.filter = function(selector) {
            if (typeof selector === 'function') {
                var results = [];
                for (var i = 0; i < this._elements.length; i++) {
                    if (selector(i, new Selection([this._elements[i]], this._sourceHtml))) {
                        results.push(this._elements[i]);
                    }
                }
                return new Selection(results, this._sourceHtml);
            }
            // CSS selector filter
            var results = [];
            for (var i = 0; i < this._elements.length; i++) {
                var found = JSON.parse(__html_find_in_fragment(this._elements[i].outerHtml, selector));
                if (found.length > 0) {
                    results.push(this._elements[i]);
                }
            }
            return new Selection(results, this._sourceHtml);
        };

        Selection.prototype.get = function(index) {
            if (typeof index === 'undefined') return this._elements;
            return this._elements[index];
        };

        Selection.prototype.children = function(selector) {
            var results = [];
            for (var i = 0; i < this._elements.length; i++) {
                var childSelector = selector ? ':scope > ' + selector : ':scope > *';
                var found = JSON.parse(__html_find_in_fragment(this._elements[i].outerHtml, childSelector));
                for (var j = 0; j < found.length; j++) {
                    results.push(found[j]);
                }
            }
            return new Selection(results, this._sourceHtml);
        };

        Selection.prototype.contents = function() {
            return this.children();
        };

        Selection.prototype.is = function(selector) {
            for (var i = 0; i < this._elements.length; i++) {
                var found = JSON.parse(__html_find_in_fragment(this._elements[i].outerHtml, selector));
                if (found.length > 0) return true;
            }
            return false;
        };

        Selection.prototype.hasClass = function(className) {
            for (var i = 0; i < this._elements.length; i++) {
                var cls = this._elements[i].attrs['class'] || '';
                if ((' ' + cls + ' ').indexOf(' ' + className + ' ') >= 0) return true;
            }
            return false;
        };

        Selection.prototype.val = function() {
            if (this._elements.length === 0) return undefined;
            return this._elements[0].attrs['value'];
        };

        Selection.prototype.data = function(name) {
            if (this._elements.length === 0) return undefined;
            return this._elements[0].attrs['data-' + name];
        };

        function parseHTML(html) {
            var elements = JSON.parse(__html_root_elements(html));
            var sel = new Selection(elements, html);
            // Override find to search the full document
            sel.find = function(selector) {
                var found = JSON.parse(__html_find(html, selector));
                return new Selection(found, html);
            };
            return sel;
        }

        globalThis.parseHTML = parseHTML;
        globalThis.Selection = Selection;
    "#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    fn with_ctx(f: impl FnOnce(&Ctx<'_>)) {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        ctx.with(|ctx| {
            register(&ctx).unwrap();
            f(&ctx);
        });
    }

    #[test]
    fn parse_html_and_find() {
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                var doc = parseHTML('<html><body><h1>Hello</h1><p class="intro">World</p></body></html>');
                globalThis.__h1_text = doc.find('h1').text();
                globalThis.__p_text = doc.find('p').text();
            "#,
            )
            .unwrap();

            let h1: String = ctx.eval("__h1_text").unwrap();
            assert_eq!(h1, "Hello");

            let p: String = ctx.eval("__p_text").unwrap();
            assert_eq!(p, "World");
        });
    }

    #[test]
    fn selection_attr() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<a href="https://example.com" id="link1">Click</a>');
                doc.find('a').attr('href');
            "#,
                )
                .unwrap();
            assert_eq!(result, "https://example.com");
        });
    }

    #[test]
    fn selection_size() {
        with_ctx(|ctx| {
            let count: i32 = ctx
                .eval(
                    r#"
                var doc = parseHTML('<ul><li>A</li><li>B</li><li>C</li></ul>');
                doc.find('li').size();
            "#,
                )
                .unwrap();
            assert_eq!(count, 3);
        });
    }

    #[test]
    fn selection_first_last() {
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                var doc = parseHTML('<ul><li>A</li><li>B</li><li>C</li></ul>');
                var items = doc.find('li');
                globalThis.__first = items.first().text();
                globalThis.__last = items.last().text();
            "#,
            )
            .unwrap();

            let first: String = ctx.eval("__first").unwrap();
            assert_eq!(first, "A");

            let last: String = ctx.eval("__last").unwrap();
            assert_eq!(last, "C");
        });
    }

    #[test]
    fn selection_eq() {
        with_ctx(|ctx| {
            let text: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<ul><li>A</li><li>B</li><li>C</li></ul>');
                doc.find('li').eq(1).text();
            "#,
                )
                .unwrap();
            assert_eq!(text, "B");
        });
    }

    #[test]
    fn selection_each() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<ul><li>A</li><li>B</li><li>C</li></ul>');
                var texts = [];
                doc.find('li').each(function(i, el) {
                    texts.push(el.text());
                });
                texts.join(',');
            "#,
                )
                .unwrap();
            assert_eq!(result, "A,B,C");
        });
    }

    #[test]
    fn selection_html() {
        with_ctx(|ctx| {
            let html: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<div><b>Bold</b> text</div>');
                doc.find('div').html();
            "#,
                )
                .unwrap();
            assert!(html.contains("<b>Bold</b>"));
        });
    }

    #[test]
    fn selection_has_class() {
        with_ctx(|ctx| {
            let has: bool = ctx
                .eval(
                    r#"
                var doc = parseHTML('<div class="foo bar">test</div>');
                doc.find('div').hasClass('bar');
            "#,
                )
                .unwrap();
            assert!(has);
        });
    }

    #[test]
    fn selection_data() {
        with_ctx(|ctx| {
            let val: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<div data-id="123">test</div>');
                doc.find('div').data('id');
            "#,
                )
                .unwrap();
            assert_eq!(val, "123");
        });
    }

    #[test]
    fn selection_map() {
        with_ctx(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<ul><li>A</li><li>B</li></ul>');
                doc.find('li').map(function(i, el) { return el.text(); }).join('-');
            "#,
                )
                .unwrap();
            assert_eq!(result, "A-B");
        });
    }

    #[test]
    fn empty_selection() {
        with_ctx(|ctx| {
            let size: i32 = ctx
                .eval(
                    r#"
                var doc = parseHTML('<div>hello</div>');
                doc.find('.nonexistent').size();
            "#,
                )
                .unwrap();
            assert_eq!(size, 0);
        });
    }

    #[test]
    fn nested_find() {
        with_ctx(|ctx| {
            let text: String = ctx
                .eval(
                    r#"
                var doc = parseHTML('<div class="outer"><div class="inner"><span>found</span></div></div><div class="other"><span>not this</span></div>');
                doc.find('.inner').find('span').text();
            "#,
                )
                .unwrap();
            assert_eq!(text, "found");
        });
    }
}
