use crate::storage::{atomic_write, config_dir, AppResult};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{fs, io};

/// Bounds one card's row of chips, so a devbox cannot push its sessions off
/// the screen with pull requests.
pub const PR_LIMIT: usize = 8;
const INPUT_LIMIT: usize = 300;
const SEGMENT_LIMIT: usize = 100;

const HEADER: &str = "\
# GitHub pull requests attached to a devbox on the llm-watch dashboard.
# One `host = url` per line, repeated per pull request; `#` starts a comment.
# Edited in place by the web dashboard.
";

/// A pull request attached to a devbox. `url` is canonical, so the same PR
/// pasted from a diff view and from the conversation tab is one entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PullRequest {
    pub url: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

pub fn prs_path() -> PathBuf {
    config_dir().join("prs")
}

/// Accepts a pull request URL — github.com or an Enterprise host — or the
/// `owner/repo#123` shorthand, and returns the canonical link. Trailing path,
/// query, and fragment are dropped: `/files?diff=split` is the same PR.
pub fn parse(input: &str) -> Option<PullRequest> {
    let input = input.trim();
    if input.is_empty() || input.chars().count() > INPUT_LIMIT {
        return None;
    }
    parse_url(input).or_else(|| parse_shorthand(input))
}

fn parse_url(input: &str) -> Option<PullRequest> {
    let (scheme, rest) = input.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let (host, path) = rest.split_once('/')?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    if segments.next()? != "pull" {
        return None;
    }
    // The number carries any `?query` or `#fragment` that followed it.
    let number = segments.next()?;
    let number = number
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .parse::<u64>()
        .ok()?;
    build(host, owner, repo, number)
}

fn parse_shorthand(input: &str) -> Option<PullRequest> {
    let (repository, number) = input.split_once('#')?;
    let (owner, repo) = repository.split_once('/')?;
    build("github.com", owner, repo, number.trim().parse().ok()?)
}

fn build(host: &str, owner: &str, repo: &str, number: u64) -> Option<PullRequest> {
    if number == 0 || ![host, owner, repo].into_iter().all(is_segment) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    Some(PullRequest {
        // Always https: GitHub and Enterprise both redirect there anyway, and
        // it keeps a hand-edited `http://` line from downgrading the link.
        url: format!("https://{host}/{owner}/{repo}/pull/{number}"),
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
    })
}

/// Keeps a URL to what the line format and a chip can hold: no whitespace, no
/// `#` to comment out the rest of the line, no path traversal oddities.
fn is_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= SEGMENT_LIMIT
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Reads `host = url` lines, one per pull request. Anything unparseable is
/// skipped rather than failing the poll that asked for it.
pub fn read_prs(path: &Path) -> BTreeMap<String, Vec<PullRequest>> {
    let Ok(content) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut prs: BTreeMap<String, Vec<PullRequest>> = BTreeMap::new();
    for line in content.lines() {
        let entry = line.split('#').next().unwrap_or_default().trim();
        let Some((host, url)) = entry.split_once('=') else {
            continue;
        };
        let host = host.trim();
        let Some(pr) = parse(url) else {
            continue;
        };
        let list = prs.entry(host.to_owned()).or_default();
        if list.len() < PR_LIMIT && !list.iter().any(|existing| existing.url == pr.url) {
            list.push(pr);
        }
    }
    prs
}

/// Attaches a pull request to one host. A duplicate, or one past the per-host
/// limit, leaves the list as it was; the caller reports what is now stored.
pub fn add_pr(path: &Path, host: &str, pr: &PullRequest) -> AppResult<Vec<PullRequest>> {
    update(path, host, |list| {
        if list.len() < PR_LIMIT && !list.iter().any(|existing| existing.url == pr.url) {
            list.push(pr.clone());
        }
    })
}

pub fn remove_pr(path: &Path, host: &str, pr: &PullRequest) -> AppResult<Vec<PullRequest>> {
    update(path, host, |list| {
        list.retain(|existing| existing.url != pr.url);
    })
}

/// Rewrites the file with one host's list changed. Other hosts keep theirs, so
/// two dashboards editing different hosts cannot erase each other.
fn update(
    path: &Path,
    host: &str,
    change: impl FnOnce(&mut Vec<PullRequest>),
) -> AppResult<Vec<PullRequest>> {
    let host = host.trim();
    if host.is_empty() {
        return Err("host must not be empty".into());
    }
    let mut prs = read_prs(path);
    let mut list = prs.remove(host).unwrap_or_default();
    change(&mut list);
    if !list.is_empty() {
        prs.insert(host.to_owned(), list.clone());
    }
    atomic_write(path, |writer| {
        writer.write_all(HEADER.as_bytes())?;
        for (host, list) in &prs {
            for pr in list {
                writeln!(writer, "{host} = {}", pr.url)?;
            }
        }
        Ok(())
    })
    .map_err(|error| io::Error::other(format!("could not write {}: {error}", path.display())))?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::temporary_test_dir;

    #[test]
    fn a_pull_request_link_is_canonicalized() {
        let pr = parse("https://github.com/canva/canva/pull/1234").unwrap();
        assert_eq!(pr.url, "https://github.com/canva/canva/pull/1234");
        assert_eq!((pr.repo.as_str(), pr.number), ("canva", 1234));

        // What you actually copy: a diff view, with a query and a fragment.
        assert_eq!(
            parse("https://github.com/canva/canva/pull/1234/files?diff=split#r99")
                .unwrap()
                .url,
            pr.url,
            "the same PR from any tab is one entry"
        );
        // An Enterprise host is kept; the shorthand assumes github.com.
        assert_eq!(
            parse("https://github.canva.dev/eng/tools/pull/7")
                .unwrap()
                .url,
            "https://github.canva.dev/eng/tools/pull/7"
        );
        assert_eq!(parse("canva/canva#1234").unwrap().url, pr.url);
        // http is accepted but stored as https.
        assert_eq!(
            parse("http://github.com/canva/canva/pull/1234")
                .unwrap()
                .url,
            pr.url
        );
    }

    #[test]
    fn anything_that_is_not_a_pull_request_is_refused() {
        for input in [
            "",
            "not a url",
            "https://github.com/canva/canva",
            "https://github.com/canva/canva/issues/12",
            "https://github.com/canva/canva/pull/nope",
            "https://github.com/canva/canva/pull/0",
            "javascript:alert(1)//github.com/a/b/pull/1",
            // A crafted line must not be able to forge a second entry or a comment.
            "https://github.com/canva/can va/pull/1",
            "https://github.com/../x/pull/1",
        ] {
            assert!(parse(input).is_none(), "{input} should not parse");
        }
        assert!(parse(&format!("https://github.com/a/b/pull/{}", "1".repeat(400))).is_none());
    }

    #[test]
    fn prs_round_trip_and_ignore_junk() {
        let root = temporary_test_dir("prs");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("prs");
        fs::write(
            &path,
            "# heading\n\
             coder.dev2 = https://github.com/canva/canva/pull/1\n\
             coder.dev2 = https://github.com/canva/canva/pull/1/files\n\
             coder.dev2 = not-a-pr\n\
             broken line\n\
             coder.dev3 = https://github.com/canva/tools/pull/9\n\
             coder.dev4 = canva/tools#9\n",
        )
        .unwrap();

        let prs = read_prs(&path);
        assert_eq!(prs["coder.dev2"].len(), 1, "the duplicate collapses");
        assert_eq!(prs["coder.dev3"][0].number, 9);
        // `#` opens a comment, so the shorthand is an input convenience only:
        // the file itself always holds canonical URLs.
        assert!(!prs.contains_key("coder.dev4"));

        let added = add_pr(&path, "coder.dev2", &parse("canva/canva#2").unwrap()).unwrap();
        assert_eq!(added.len(), 2);
        assert_eq!(
            add_pr(&path, "coder.dev2", &parse("canva/canva#2").unwrap())
                .unwrap()
                .len(),
            2,
            "adding the same PR twice is a no-op"
        );

        let left = remove_pr(&path, "coder.dev2", &parse("canva/canva#1").unwrap()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].number, 2);
        let prs = read_prs(&path);
        assert_eq!(prs["coder.dev3"].len(), 1, "other hosts kept");

        // Emptying a host drops it from the file rather than leaving a stub.
        remove_pr(&path, "coder.dev2", &parse("canva/canva#2").unwrap()).unwrap();
        assert!(!read_prs(&path).contains_key("coder.dev2"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_host_cannot_collect_unbounded_pull_requests() {
        let root = temporary_test_dir("prs-limit");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("prs");
        for number in 1..=(PR_LIMIT + 3) {
            add_pr(
                &path,
                "coder.dev2",
                &parse(&format!("canva/canva#{number}")).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(read_prs(&path)["coder.dev2"].len(), PR_LIMIT);
        assert!(add_pr(
            Path::new("/nonexistent/x/prs"),
            "",
            &parse("a/b#1").unwrap()
        )
        .is_err());
        let _ = fs::remove_dir_all(root);
    }
}
