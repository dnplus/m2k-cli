use super::{block, ACCENT, SELECTED};
use crate::{
    directory::{self, DirectoryContact},
    directory_chart::{self, Department, Member, OrgChart},
    model::Kind,
    store::Store,
};
use anyhow::Result;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 聯絡人頁的組織圖檢視；樹由 `directory_chart::build` 建立，與 `m2k directory chart` 同源。
pub struct OrgChartView {
    chart: OrgChart,
    contacts: HashMap<String, DirectoryContact>,
    expanded: HashSet<String>,
    list: ListState,
    /// 窄版面時以全畫面顯示選取成員的詳情。
    pub detail: bool,
}

enum Node<'a> {
    Department(&'a Department),
    Member(&'a Member),
}

impl OrgChartView {
    /// 只讀本機快取；第二個值是快取不完整時要顯示在狀態列的原因。
    pub fn load(dir: &Path) -> Result<(Self, Option<String>)> {
        let store = Store::open_reader(dir)?;
        let nodes = directory::address_book_tree(&store.collections(Kind::Contacts)?, None);
        let contacts = directory::merge_contact_items(
            &store.list(Kind::Contacts, None, None, usize::MAX)?,
            &nodes,
        );
        let chart = directory_chart::build(&nodes, &contacts, None)?;
        let warning = if chart.departments.is_empty() {
            Some("組織圖：本機沒有通訊錄快取 · 按 Shift-R 同步公司目錄".to_owned())
        } else if chart.member_count == 0 {
            Some("組織圖：通訊錄快取沒有成員 · 按 Shift-R 同步公司目錄".to_owned())
        } else if chart
            .departments
            .iter()
            .all(|root| root.children.is_empty())
        {
            Some(
                "組織圖：快取沒有公司根與子部門，只列出扁平通訊錄 · 按 Shift-R 同步公司目錄"
                    .to_owned(),
            )
        } else {
            None
        };
        let expanded = chart
            .departments
            .iter()
            .map(|root| root.path.clone())
            .collect();
        let view = Self {
            contacts: contacts
                .into_iter()
                .map(|contact| (contact.id.clone(), contact))
                .collect(),
            chart,
            expanded,
            list: ListState::default().with_selected(Some(0)),
            detail: false,
        };
        Ok((view, warning))
    }

    pub fn summary(&self) -> String {
        format!(
            "組織圖 · 部門 {} 個 · 成員 {} 位 · Enter 展開／收合 · v 回列表",
            self.chart.department_count, self.chart.member_count
        )
    }

    fn nodes(&self) -> Vec<(usize, Node<'_>)> {
        fn walk<'a>(
            out: &mut Vec<(usize, Node<'a>)>,
            department: &'a Department,
            level: usize,
            expanded: &HashSet<String>,
        ) {
            out.push((level, Node::Department(department)));
            if !expanded.contains(&department.path) {
                return;
            }
            for member in &department.members {
                out.push((level + 1, Node::Member(member)));
            }
            for child in &department.children {
                walk(out, child, level + 1, expanded);
            }
        }
        let mut out = Vec::new();
        for root in &self.chart.departments {
            walk(&mut out, root, 0, &self.expanded);
        }
        out
    }

    pub fn move_selection(&mut self, delta: i64) {
        let len = self.nodes().len();
        if len == 0 {
            return;
        }
        let index = self.list.selected().unwrap_or(0) as i64 + delta;
        self.list
            .select(Some(index.clamp(0, len as i64 - 1) as usize));
    }

    pub fn select_edge(&mut self, last: bool) {
        let len = self.nodes().len();
        self.list
            .select(Some(if last { len.saturating_sub(1) } else { 0 }));
    }

    /// Enter：部門展開或收合；成員則開啟詳情。
    pub fn activate(&mut self) {
        let index = self.list.selected().unwrap_or(0);
        let path = match self.nodes().get(index) {
            Some((_, Node::Department(department))) => department.path.clone(),
            Some((_, Node::Member(_))) => {
                self.detail = true;
                return;
            }
            None => return,
        };
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
    }

    pub fn detail_text(&self) -> String {
        let nodes = self.nodes();
        match nodes.get(self.list.selected().unwrap_or(0)) {
            Some((_, Node::Member(member))) => self
                .contacts
                .get(member.id())
                .map(directory::format_contact_detail)
                .unwrap_or_else(|| member.name.clone()),
            Some((_, Node::Department(department))) => format!(
                "{}\n路徑：{}\n直屬成員 {} 人 · 子部門 {} 個\n\n{}",
                department.name,
                department.path,
                department.members.len(),
                department.children.len(),
                self.chart.reporting_note
            ),
            None => "本機沒有可顯示的組織圖。\n\n按 Shift-R 同步公司目錄。".to_owned(),
        }
    }

    pub fn draw_tree(&mut self, frame: &mut Frame, area: Rect) {
        let items = self
            .nodes()
            .into_iter()
            .map(|(level, node)| {
                let indent = "  ".repeat(level);
                ListItem::new(match node {
                    Node::Department(department) => {
                        let open =
                            if department.members.is_empty() && department.children.is_empty() {
                                "-"
                            } else if self.expanded.contains(&department.path) {
                                "v"
                            } else {
                                ">"
                            };
                        format!(
                            "{indent}[{open}] {}（{} 人）",
                            department.name,
                            department.members.len()
                        )
                    }
                    Node::Member(member) => {
                        let mut line = format!("{indent}{}", member.name);
                        if let Some(title) = &member.title {
                            line.push_str(&format!(" · {title}"));
                        }
                        line
                    }
                })
            })
            .collect::<Vec<_>>();
        let panel = block(format!(
            " 聯絡人 · 組織圖 · {} 個部門 ",
            self.chart.department_count
        ));
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new("本機沒有可顯示的組織圖。\n\n按 Shift-R 同步公司目錄；v 回列表。")
                    .block(panel)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let list = List::new(items)
            .block(panel)
            .highlight_style(
                Style::default()
                    .fg(ACCENT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut self.list);
    }

    pub fn draw_detail(&self, frame: &mut Frame, area: Rect, scroll: u16) {
        frame.render_widget(
            Paragraph::new(self.detail_text())
                .block(block(if self.detail {
                    " 詳情 · Esc 返回組織圖 "
                } else {
                    " 詳情預覽 "
                }))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }
}
