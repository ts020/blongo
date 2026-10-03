//! The file browser and git bar of a thread's folder.
//!
//! Everything comes from workspace queries, so it works the same for a
//! remote environment: folders are listed when opened (ignored files left
//! out by the backend), a file is read up to a cap and drawn with a
//! virtualized list, and the git bar shows the branch and changes and can
//! switch or create a branch and commit.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::ThreadId;
use blongo_protocol::workspace::{
    BranchInfo, DirEntry, FileContent, GitStatusInfo, Query, QueryReply,
};
use gpui::{
    Context, Entity, Focusable, FontWeight, SharedString, Subscription, UniformListScrollHandle,
    Window, div, prelude::*, px, uniform_list,
};

use crate::input::{InputEvent, TextInput};
use crate::theme;
use crate::timeline::button;

const ROW: f32 = 20.;
const READ_CAP: u32 = 512 * 1024;
const TREE_WIDTH: f32 = 260.;

#[derive(Clone)]
struct TreeRow {
    path: String,
    name: SharedString,
    depth: usize,
    is_dir: bool,
}

pub struct FilesView {
    backend: Arc<dyn Backend>,
    thread_id: ThreadId,
    /// Folder path ("" is the top) → its entries, once listed.
    dirs: HashMap<String, Vec<DirEntry>>,
    expanded: HashSet<String>,
    rows: Vec<TreeRow>,
    open: Option<FileContent>,
    lines: Vec<SharedString>,
    pub status: Option<GitStatusInfo>,
    branches: Option<Vec<BranchInfo>>,
    branch_menu: bool,
    branch_input: Entity<TextInput>,
    commit_input: Entity<TextInput>,
    /// Last git result or error.
    pub message: Option<(bool, SharedString)>,
    tree_scroll: UniformListScrollHandle,
    file_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl FilesView {
    pub fn new(
        backend: Arc<dyn Backend>,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch_input = cx.new(|cx| TextInput::new("new-branch-name", false, cx));
        let commit_input = cx.new(|cx| TextInput::new("Commit message", false, cx));
        let subscriptions = vec![
            cx.subscribe_in(&branch_input, window, |this, input, event, _, cx| {
                if let InputEvent::Submit = event {
                    let name = input.read(cx).text().trim().to_owned();
                    if !name.is_empty() {
                        this.switch(name, true, cx);
                    }
                }
            }),
            cx.subscribe_in(&commit_input, window, |this, _, event, _, cx| {
                if let InputEvent::Submit = event {
                    this.commit(cx);
                }
            }),
        ];
        let mut this = Self {
            backend,
            thread_id,
            dirs: HashMap::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            open: None,
            lines: Vec::new(),
            status: None,
            branches: None,
            branch_menu: false,
            branch_input,
            commit_input,
            message: None,
            tree_scroll: UniformListScrollHandle::new(),
            file_scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.expanded.insert(String::new());
        this.list("", cx);
        this.refresh_git(cx);
        this
    }

    fn ask(
        &self,
        query: Query,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, Result<QueryReply, String>, &mut Context<Self>) + 'static,
    ) {
        crate::query::ask(&self.backend, query, cx.weak_entity(), cx, f);
    }

    fn list(&mut self, path: &str, cx: &mut Context<Self>) {
        let owned = path.to_owned();
        self.ask(
            Query::ListDir {
                thread_id: self.thread_id,
                path: path.to_owned(),
            },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(QueryReply::Dir(entries)) => {
                        this.dirs.insert(owned, entries);
                    }
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                this.rebuild(cx);
            },
        );
    }

    pub fn refresh_git(&mut self, cx: &mut Context<Self>) {
        self.ask(
            Query::GitStatus {
                thread_id: self.thread_id,
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(QueryReply::GitStatus(status)) => this.status = Some(status),
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            },
        );
    }

    fn load_branches(&mut self, cx: &mut Context<Self>) {
        self.ask(
            Query::GitBranches {
                thread_id: self.thread_id,
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(QueryReply::Branches(b)) => this.branches = Some(b),
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            },
        );
    }

    pub fn switch(&mut self, branch: String, create: bool, cx: &mut Context<Self>) {
        self.branch_menu = false;
        self.message = Some((true, format!("Switching to {branch}…").into()));
        self.ask(
            Query::GitSwitch {
                thread_id: self.thread_id,
                branch,
                create,
            },
            cx,
            |this, result, cx| {
                this.message = Some(done(result));
                this.branch_input.update(cx, |i, cx| i.set_text("", cx));
                this.reload(cx);
            },
        );
        cx.notify();
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).text().trim().to_owned();
        if message.is_empty() {
            self.message = Some((false, "Type a commit message first".into()));
            cx.notify();
            return;
        }
        self.ask(
            Query::GitCommit {
                thread_id: self.thread_id,
                message,
            },
            cx,
            |this, result, cx| {
                let ok = result.is_ok();
                this.message = Some(done(result));
                if ok {
                    this.commit_input.update(cx, |i, cx| i.set_text("", cx));
                }
                this.refresh_git(cx);
            },
        );
    }

    /// Re-read the listed folders, the open file and git.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let listed: Vec<String> = self.dirs.keys().cloned().collect();
        self.dirs.clear();
        let open: Vec<String> = listed
            .into_iter()
            .filter(|p| self.expanded.contains(p))
            .collect();
        for path in &open {
            self.list(path, cx);
        }
        if let Some(path) = self.open.as_ref().map(|f| f.path.clone()) {
            self.open_file(path, cx);
        }
        self.refresh_git(cx);
        self.rebuild(cx);
    }

    pub fn open_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.ask(
            Query::ReadFile {
                thread_id: self.thread_id,
                path,
                max_bytes: READ_CAP,
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(QueryReply::File(file)) => {
                        this.lines = if file.binary {
                            vec!["(binary file)".into()]
                        } else {
                            let mut lines: Vec<SharedString> = file
                                .text
                                .lines()
                                .map(|l| SharedString::from(l.replace('\t', "    ")))
                                .collect();
                            if file.truncated {
                                lines.push("… (cut: the file is larger)".into());
                            }
                            lines
                        };
                        this.open = Some(file);
                        this.file_scroll
                            .scroll_to_item(0, gpui::ScrollStrategy::Top);
                    }
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            },
        );
    }

    fn toggle_dir(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            if !self.dirs.contains_key(&path) {
                self.list(&path, cx);
            }
        }
        self.rebuild(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        fn walk(this: &FilesView, dir: &str, depth: usize, out: &mut Vec<TreeRow>) {
            let Some(entries) = this.dirs.get(dir) else {
                return;
            };
            for e in entries {
                let path = if dir.is_empty() {
                    e.name.clone()
                } else {
                    format!("{dir}/{}", e.name)
                };
                out.push(TreeRow {
                    path: path.clone(),
                    name: e.name.clone().into(),
                    depth,
                    is_dir: e.is_dir,
                });
                if e.is_dir && this.expanded.contains(&path) {
                    walk(this, &path, depth + 1, out);
                }
            }
        }
        let mut rows = Vec::new();
        walk(self, "", 0, &mut rows);
        self.rows = rows;
        cx.notify();
    }

    fn render_tree(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let open = self.open.as_ref().map(|f| f.path.as_str());
        range
            .filter_map(|ix| self.rows.get(ix).map(|r| (ix, r.clone())))
            .map(|(ix, row)| {
                let expanded = self.expanded.contains(&row.path);
                let selected = open == Some(row.path.as_str());
                let path = row.path.clone();
                div()
                    .id(("tree", ix))
                    .h(px(ROW))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(8. + 12. * row.depth as f32))
                    .pr_2()
                    .text_xs()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::surface_hover()))
                    .hover(|d| d.bg(theme::surface_hover()))
                    .child(div().w(px(10.)).text_color(theme::text_faint()).child(
                        match (row.is_dir, expanded) {
                            (true, true) => "▾",
                            (true, false) => "▸",
                            _ => "",
                        },
                    ))
                    .child(
                        div()
                            .text_color(if row.is_dir {
                                theme::text()
                            } else {
                                theme::text_muted()
                            })
                            .child(row.name.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if row.is_dir {
                            this.toggle_dir(path.clone(), cx);
                        } else {
                            this.open_file(path.clone(), cx);
                        }
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn render_lines(
        &mut self,
        range: Range<usize>,
        _cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        range
            .filter_map(|ix| self.lines.get(ix).map(|l| (ix, l.clone())))
            .map(|(ix, line)| {
                div()
                    .h(px(ROW))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .font_family(theme::MONO)
                    .text_xs()
                    .child(
                        div()
                            .w(px(48.))
                            .flex_shrink_0()
                            .text_right()
                            .pr_3()
                            .text_color(theme::text_faint())
                            .child((ix + 1).to_string()),
                    )
                    .child(line)
                    .into_any_element()
            })
            .collect()
    }

    fn render_git_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch: SharedString = match &self.status {
            Some(s) => s
                .branch
                .clone()
                .unwrap_or_else(|| "(detached)".into())
                .into(),
            None => "…".into(),
        };
        let detail: SharedString = match &self.status {
            Some(s) => {
                let mut t = format!("{} changed", s.changes.len());
                if s.ahead > 0 || s.behind > 0 {
                    t.push_str(&format!("  ↑{} ↓{}", s.ahead, s.behind));
                }
                t.into()
            }
            None => "".into(),
        };
        div()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .child(
                div()
                    .id("branch-button")
                    .flex()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme::surface_hover())
                    .cursor_pointer()
                    .child("⑂")
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(branch))
                    .child("▾")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.branch_menu = !this.branch_menu;
                        if this.branch_menu {
                            this.load_branches(cx);
                            window.focus(&this.branch_input.focus_handle(cx), cx);
                        }
                        cx.notify();
                    })),
            )
            .child(div().text_color(theme::text_muted()).child(detail))
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme::code_bg())
                    .child(self.commit_input.clone()),
            )
            .child(button(
                "git-commit".into(),
                "Commit all",
                theme::accent_bg(),
                theme::text(),
                cx.listener(|this, _, _, cx| this.commit(cx)),
            ))
            .child(button(
                "git-refresh".into(),
                "Refresh",
                theme::surface_hover(),
                theme::text(),
                cx.listener(|this, _, _, cx| this.reload(cx)),
            ))
            .when(self.branch_menu, |d| {
                d.child(gpui::deferred(self.render_branch_menu(cx)).with_priority(1))
            })
    }

    fn render_branch_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut menu = div()
            .id("branch-menu")
            .occlude()
            .absolute()
            .top(px(30.))
            .left(px(8.))
            .w(px(320.))
            .max_h(px(360.))
            .overflow_y_scroll()
            .p_1()
            .rounded_md()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .text_xs()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_color(theme::text_muted())
                    .child("Create a branch from HEAD (Enter):"),
            )
            .child(
                div()
                    .mx_2()
                    .mb_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme::code_bg())
                    .child(self.branch_input.clone()),
            );
        match &self.branches {
            None => menu = menu.child(div().px_2().py_1().child("Loading branches…")),
            Some(branches) => {
                for (ix, b) in branches.iter().enumerate() {
                    let name = b.name.clone();
                    menu = menu.child(
                        div()
                            .id(("branch", ix))
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .flex()
                            .justify_between()
                            .cursor_pointer()
                            .hover(|d| d.bg(theme::surface_hover()))
                            .child(SharedString::from(b.name.clone()))
                            .when(b.current, |d| {
                                d.child(div().text_color(theme::accent()).child("✓"))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.switch(name.clone(), false, cx)
                            })),
                    );
                }
            }
        }
        menu
    }
}

fn done(result: Result<QueryReply, String>) -> (bool, SharedString) {
    match result {
        Ok(QueryReply::Done(text)) => (true, text.into()),
        Ok(_) => (true, "Done".into()),
        Err(err) => (false, err.into()),
    }
}

impl Render for FilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let file_title: SharedString = self
            .open
            .as_ref()
            .map_or("Pick a file (or Ctrl+P to search)".into(), |f| {
                f.path.clone().into()
            });
        let changes = self
            .status
            .as_ref()
            .map(|s| s.changes.clone())
            .unwrap_or_default();
        let tree = div()
            .w(px(TREE_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme::border())
            .when(!changes.is_empty(), |d| {
                d.child(
                    div()
                        .id("changes")
                        .max_h(px(140.))
                        .overflow_y_scroll()
                        .border_b_1()
                        .border_color(theme::border())
                        .py_1()
                        .child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(theme::text_faint())
                                .child("CHANGES"),
                        )
                        .children(changes.into_iter().take(200).enumerate().map(
                            |(ix, (code, path))| {
                                let open = path.clone();
                                div()
                                    .id(("change", ix))
                                    .px_2()
                                    .flex()
                                    .gap_2()
                                    .text_xs()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme::surface_hover()))
                                    .child(
                                        div()
                                            .w(px(16.))
                                            .font_family(theme::MONO)
                                            .text_color(theme::warning())
                                            .child(code),
                                    )
                                    .child(path)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_file(open.clone(), cx)
                                    }))
                            },
                        )),
                )
            })
            .child(
                div().flex_1().min_h_0().child(
                    uniform_list(
                        "file-tree",
                        self.rows.len(),
                        cx.processor(|this, range, _, cx| this.render_tree(range, cx)),
                    )
                    .track_scroll(&self.tree_scroll)
                    .size_full(),
                ),
            );
        let content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(file_title),
            )
            .child(
                div().flex_1().min_h_0().bg(theme::code_bg()).child(
                    uniform_list(
                        "file-lines",
                        self.lines.len(),
                        cx.processor(|this, range, _, cx| this.render_lines(range, cx)),
                    )
                    .track_scroll(&self.file_scroll)
                    .size_full(),
                ),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_git_bar(cx))
            .when_some(self.message.clone(), |d, (ok, text)| {
                d.child(
                    div()
                        .px_3()
                        .py_0p5()
                        .text_xs()
                        .text_color(if ok {
                            theme::text_muted()
                        } else {
                            theme::danger()
                        })
                        .child(text),
                )
            })
            .child(div().flex_1().min_h_0().flex().child(tree).child(content))
    }
}
