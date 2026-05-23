use crate::selector::TextQuoteSelector;
use crate::store::Comment;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize)]
pub struct ReviewFile {
    pub review_comments: Vec<ReviewComment>,
    pub files: HashMap<String, FileEntry>,
}

#[derive(Serialize)]
pub struct FileEntry {
    pub comments: Vec<FileComment>,
}

#[derive(Serialize)]
pub struct ReviewComment {
    pub id: String,
    pub body: String,
    pub scope: &'static str,
    pub author: String,
    pub resolved: bool,
    pub replies: Vec<Reply>,
}

#[derive(Serialize)]
pub struct FileComment {
    pub id: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    pub quote: String,
    pub anchor: TextQuoteSelector,
    pub author: String,
    pub resolved: bool,
    pub replies: Vec<Reply>,
}

#[derive(Serialize)]
pub struct Reply {
    pub id: String,
    pub body: String,
    pub author: String,
}

pub fn build(slug: &str, comments: Vec<Comment>) -> ReviewFile {
    let mut tops: Vec<Comment> = Vec::new();
    let mut replies_by_parent: HashMap<String, Vec<Comment>> = HashMap::new();
    for c in comments {
        match &c.parent_id {
            Some(pid) => replies_by_parent.entry(pid.clone()).or_default().push(c),
            None => tops.push(c),
        }
    }

    let file_path = format!("{slug}.html");
    let mut file_comments = Vec::new();
    for c in tops {
        let reply_list = replies_by_parent
            .remove(&c.id)
            .unwrap_or_default()
            .into_iter()
            .map(|r| Reply {
                id: r.id,
                body: r.body,
                author: r.author,
            })
            .collect();
        file_comments.push(FileComment {
            id: c.id,
            start_line: 0,
            end_line: 0,
            quote: c.selector.exact.clone(),
            anchor: c.selector,
            body: c.body,
            author: c.author,
            resolved: c.resolved,
            replies: reply_list,
        });
    }

    let mut files = HashMap::new();
    files.insert(
        file_path,
        FileEntry {
            comments: file_comments,
        },
    );

    ReviewFile {
        review_comments: Vec::new(),
        files,
    }
}
