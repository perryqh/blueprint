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
    /// Always 0, both of them. The shape is inherited from a line-oriented
    /// review format, but a blueprint comment anchors to a *quote*, not to a
    /// line — the HTML it points into is generated and reflows on every edit, so
    /// any line number we computed would be wrong by the next update. `anchor`
    /// and `quote` carry the real position. Kept in the payload rather than
    /// dropped so consumers expecting the format's field set still parse.
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

/// Collect `parent`'s replies, and *their* replies, in depth-first order.
///
/// Flattening the whole subtree rather than taking one level is what stops
/// replies from being silently dropped. The frontend's thread renderer recurses
/// (`render.js` calls itself with `byParent.get(reply.id)`), so a reviewer can
/// and does reply to a reply — and every one of those used to be invisible in
/// the file Claude reads, because only direct children of a top-level comment
/// were ever drained. A comment the reviewer can see and the agent cannot is the
/// worst possible failure mode for this file.
///
/// The output stays flat because `Reply` has no children field: the review file
/// is a transcript for an agent, and "who replied to whom" three levels down has
/// never been part of what it promises. Depth-first ordering keeps each
/// sub-thread contiguous, which is the part that carries meaning.
fn drain_reply_subtree(
    parent_id: &str,
    replies_by_parent: &mut HashMap<String, Vec<Comment>>,
    out: &mut Vec<Reply>,
) {
    for r in replies_by_parent.remove(parent_id).unwrap_or_default() {
        let id = r.id.clone();
        out.push(Reply {
            id: r.id,
            body: r.body,
            author: r.author,
        });
        drain_reply_subtree(&id, replies_by_parent, out);
    }
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
        let mut reply_list = Vec::new();
        drain_reply_subtree(&c.id, &mut replies_by_parent, &mut reply_list);
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
