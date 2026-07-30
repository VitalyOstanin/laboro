//! GitHub backend driven through the `gh` CLI.
//!
//! Lists my open issues and pull requests and my notifications (read and unread,
//! via `all=true`, each tagged with a `read` flag), and marks notifications read
//! (`PATCH`/`PUT`); GitHub's REST API has no mark-unread, so read is one-way.
//! `gh` handles authentication and host selection, so no token is stored here.
//! Output is normalized to the same shape as an OpenProject work package so the
//! CLI and GUI can render both backends uniformly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;

use crate::entities;
use crate::error::Error;

/// GitHub's REST API caps `per_page` at 100; request the maximum to keep the
/// number of round-trips down. Shared with the update checker ([`crate::update`]).
pub const GITHUB_MAX_PER_PAGE: u32 = 100;

/// Availability of the `gh` CLI, which the GitHub task backend requires. The
/// update checker does NOT use `gh` (it reads public releases anonymously), so
/// this only matters when a GitHub server is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GhStatus {
    /// `gh` is installed and authenticated — the backend can run.
    Ready,
    /// `gh` is not on `PATH`; it must be installed.
    Missing,
    /// `gh` is installed but not logged in; `gh auth login` is needed.
    Unauthenticated,
}

/// Classify `gh` availability by probing a [`GhRunner`]: the binary must be
/// present (`gh --version`) and authenticated (`gh auth status`). Separated from
/// process spawning so it is unit-tested with a fake runner.
pub fn gh_status<R: GhRunner>(runner: &R) -> GhStatus {
    match runner.run(&["--version"]) {
        Ok(_) => {}
        // A spawn failure means the binary is absent from PATH.
        Err(Error::Io(_)) => return GhStatus::Missing,
        // Any other failure of `--version` means `gh` is unusable here.
        Err(_) => return GhStatus::Missing,
    }
    match runner.run(&["auth", "status"]) {
        Ok(_) => GhStatus::Ready,
        Err(_) => GhStatus::Unauthenticated,
    }
}

/// Probe the real `gh` for `host` (empty = default host). Convenience over
/// [`gh_status`] with a [`GhCli`] runner.
pub fn gh_status_for_host(host: &str) -> GhStatus {
    gh_status(&GhCli {
        host: host.to_owned(),
    })
}

/// The authenticated `gh` account: which login on which host. Shown in the setup
/// wizard so the user sees *who* and *where* `gh` is signed in, not just that it
/// is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GhAccount {
    /// Authenticated user's login on this host.
    pub login: String,
    /// Host the login belongs to (`github.com` for the default host).
    pub host: String,
}

/// Read the authenticated account for `host` via a [`GhRunner`]. The login comes
/// from `gh api user`; the host echoes the probed host (`github.com` when empty).
/// Separated from process spawning so it is unit-tested with a fake runner.
pub fn gh_account<R: GhRunner>(runner: &R, host: &str) -> Result<GhAccount, Error> {
    let raw = runner.run(&["api", "user", "--jq", ".login"])?;
    let login = String::from_utf8_lossy(&raw).trim().to_string();
    let host = if host.is_empty() { "github.com" } else { host };
    Ok(GhAccount {
        login,
        host: host.to_owned(),
    })
}

/// Read the authenticated account for `host` from the real `gh`. Convenience
/// over [`gh_account`] with a [`GhCli`] runner.
pub fn gh_account_for_host(host: &str) -> Result<GhAccount, Error> {
    gh_account(
        &GhCli {
            host: host.to_owned(),
        },
        host,
    )
}

/// How many `gh` processes may run at once in [`GhRunner::run_batch`]. Every
/// invocation is its own process, and the count of batched calls follows the
/// data (one per repository with a CI notification), so the fan-out is capped
/// here rather than left to grow with the inbox. Ten is far below GitHub's
/// hundred-concurrent-request secondary limit while covering the usual batch in
/// a single wave.
const MAX_PARALLEL_GH: usize = 10;

/// Abstraction over invoking `gh`, so tests can feed fixtures instead of
/// spawning the real process.
pub trait GhRunner {
    /// Run `gh` with `args`, returning captured stdout on success.
    fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error>;

    /// Run independent invocations, returning one result per call in input
    /// order. The default is sequential; [`GhCli`] overrides it with a bounded
    /// parallel version. Order is part of the contract: callers rely on it to
    /// keep result priority (see [`GithubBackend::list_my_tasks`]).
    fn run_batch(&self, calls: &[Vec<String>]) -> Vec<Result<Vec<u8>, Error>> {
        calls.iter().map(|c| self.run(&as_args(c))).collect()
    }

    /// Key identifying the account this runner talks to, for process-wide
    /// caches. `None` disables caching, which is what fakes want so tests never
    /// observe each other's entries.
    fn cache_key(&self) -> Option<&str> {
        None
    }
}

/// A reference runs like the runner it points at, so a caller can keep ownership
/// (and inspect the runner afterwards) while handing it to a backend.
impl<T: GhRunner + ?Sized> GhRunner for &T {
    fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        (**self).run(args)
    }

    fn run_batch(&self, calls: &[Vec<String>]) -> Vec<Result<Vec<u8>, Error>> {
        (**self).run_batch(calls)
    }

    fn cache_key(&self) -> Option<&str> {
        (**self).cache_key()
    }
}

/// Borrow an owned argument vector as the `&[&str]` slice [`GhRunner::run`] takes.
fn as_args(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Real runner: spawns `gh`, pinning the host via `GH_HOST` (github.com or an
/// enterprise host). Command/flag specifics are verified against the installed
/// `gh` at integration time.
pub struct GhCli {
    pub host: String,
}

impl GhRunner for GhCli {
    fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        let mut cmd = std::process::Command::new("gh");
        cmd.args(args);
        if !self.host.is_empty() {
            cmd.env("GH_HOST", &self.host);
        }
        // Hand the token down explicitly so each `gh` skips its own keyring read.
        // Concurrent reads serialize on the Secret Service: fifteen parallel calls
        // measured 5.7 s reading the keyring against 2.1 s with the token in the
        // environment.
        if let Some(token) = cached_token(&self.host) {
            cmd.env("GH_TOKEN", token);
        }
        let out = cmd
            .output()
            .map_err(|e| Error::Io(format!("spawn gh: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Api(format!(
                "gh {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Spawn the calls concurrently, at most [`MAX_PARALLEL_GH`] at a time.
    /// Workers pull from a shared cursor rather than taking a fixed slice each,
    /// so a slow repository does not hold back the rest of the batch.
    fn run_batch(&self, calls: &[Vec<String>]) -> Vec<Result<Vec<u8>, Error>> {
        /// One batch entry's result, filled by whichever worker took it.
        type Slot = Mutex<Option<Result<Vec<u8>, Error>>>;
        let slots: Vec<Slot> = calls.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let workers = calls.len().min(MAX_PARALLEL_GH);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(call) = calls.get(i) else { break };
                    let result = self.run(&as_args(call));
                    *slots[i].lock().expect("gh batch slot poisoned") = Some(result);
                });
            }
        });
        slots
            .into_iter()
            .map(|s| {
                s.into_inner()
                    .expect("gh batch slot poisoned")
                    .expect("every batch slot is filled before the scope ends")
            })
            .collect()
    }

    fn cache_key(&self) -> Option<&str> {
        Some(if self.host.is_empty() {
            "github.com"
        } else {
            &self.host
        })
    }
}

/// Whether a searched item is an issue or a pull request; maps to the typed
/// [`entities::TaskKind`] on the produced task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Issue,
    PullRequest,
}

/// Join assignee logins into a single `", "`-separated string, or `Null` when
/// there are none.
fn assignees_label(v: &Value) -> Value {
    let logins: Vec<&str> = v
        .get("assignees")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("login").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if logins.is_empty() {
        Value::Null
    } else {
        Value::from(logins.join(", "))
    }
}

/// Assignees as a comma-joined string, or `None` when there are none.
fn assignees_string(v: &Value) -> Option<String> {
    match assignees_label(v) {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// Map a GitHub issue/PR `state` (`open` / `closed` / `merged`) to a normalized
/// status bucket.
fn status_category_from_state(state: Option<&str>) -> entities::StatusCategory {
    match state {
        Some(s) if s.eq_ignore_ascii_case("open") => entities::StatusCategory::Open,
        Some(s) if s.eq_ignore_ascii_case("closed") || s.eq_ignore_ascii_case("merged") => {
            entities::StatusCategory::Done
        }
        _ => entities::StatusCategory::Unknown,
    }
}

/// Build a typed [`entities::Task`] from one `gh search issues`/`prs` element.
/// `reason` is supplied by the caller, which knows the search that produced the
/// item (involves / review-requested / owner); GitHub search does not tag it.
pub fn task_from_gh(v: &Value, kind: TaskKind, reason: entities::InboxReason) -> entities::Task {
    let repo = v
        .get("repository")
        .and_then(|r| r.get("nameWithOwner"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let number = v.get("number").and_then(Value::as_i64);
    let display = match number {
        Some(n) if !repo.is_empty() => format!("{repo}#{n}"),
        Some(n) => n.to_string(),
        None => String::new(),
    };
    let raw = number.map(|n| n.to_string()).unwrap_or_default();
    let state = v.get("state").and_then(Value::as_str);
    entities::Task {
        id: entities::TaskId { display, raw },
        kind: match kind {
            TaskKind::Issue => entities::TaskKind::Issue,
            TaskKind::PullRequest => entities::TaskKind::PullRequest,
        },
        reason,
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        url: v.get("url").and_then(Value::as_str).map(str::to_owned),
        status: state.map(str::to_owned),
        status_category: status_category_from_state(state),
        project: (!repo.is_empty()).then(|| repo.to_owned()),
        // Set by `list_my_tasks` once the login is known (repo owner == login).
        mine: false,
        assignee: assignees_string(v),
        author: None,
        created_at: v
            .get("createdAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        updated_at: v
            .get("updatedAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        due_date: None,
        priority: None,
        labels: Vec::new(),
        custom_fields: Vec::new(),
    }
}

/// Map a GitHub notification subject type (PascalCase: `Issue`, `PullRequest`,
/// `CheckSuite`, `Discussion`, …) to a [`entities::NotifKind`].
fn notif_kind_from_subject_type(t: Option<&str>) -> entities::NotifKind {
    match t {
        Some("Issue") => entities::NotifKind::Issue,
        Some("PullRequest") => entities::NotifKind::PullRequest,
        Some("CheckSuite") => entities::NotifKind::CheckSuite,
        Some(other) => entities::NotifKind::Other(other.to_owned()),
        None => entities::NotifKind::Other(String::new()),
    }
}

/// The typed CI outcome for a check-suite title (mirrors [`check_suite_outcome`]).
fn ci_outcome_from_title(title: &str) -> entities::CiOutcome {
    match check_suite_outcome(title) {
        "failure" => entities::CiOutcome::Failure,
        "success" => entities::CiOutcome::Success,
        _ => entities::CiOutcome::Neutral,
    }
}

/// Build a typed [`entities::Notification`] from one `gh api notifications`
/// element. `url` is the browser-viewable subject address (the REST `subject.url`
/// is not viewable); a check-suite item also carries its run `outcome`.
pub fn notification_from_gh(v: &Value) -> entities::Notification {
    let subject = v.get("subject");
    let subject_type = subject.and_then(|s| s.get("type")).and_then(Value::as_str);
    let title = subject
        .and_then(|s| s.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let unread = v.get("unread").and_then(Value::as_bool).unwrap_or(true);
    let outcome = (subject_type == Some("CheckSuite")).then(|| ci_outcome_from_title(&title));
    entities::Notification {
        id: v.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
        reason: v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        kind: notif_kind_from_subject_type(subject_type),
        title,
        project: v
            .get("repository")
            .and_then(|r| r.get("full_name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: notification_html_url(v),
        updated_at: v
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        read: !unread,
        outcome,
        wp_id: None,
    }
}

/// Classify a CheckSuite notification's outcome from its subject title, which
/// GitHub phrases as "… workflow run failed/succeeded/cancelled for … branch".
/// Returns `"failure"`, `"success"`, or `"neutral"` so the UI can tint it
/// (failed → warn, succeeded → success). The `gh api notifications` payload
/// carries no structured conclusion, so the title text is the only signal.
fn check_suite_outcome(title: &str) -> &'static str {
    let t = title.to_ascii_lowercase();
    if t.contains("fail") {
        "failure"
    } else if t.contains("succe") || t.contains("passed") {
        "success"
    } else {
        "neutral"
    }
}

/// Browser URL for a notification's subject, or `None` when it cannot be built
/// (unsupported subject type, or missing repository/number). Built from the
/// repository web address so it works on GitHub Enterprise hosts too, not only
/// `github.com`.
fn notification_html_url(v: &Value) -> Option<String> {
    let subject = v.get("subject")?;
    let repo = v.get("repository")?;
    let base = repo
        .get("html_url")
        .and_then(Value::as_str)
        .map(|u| u.trim_end_matches('/').to_string())
        .or_else(|| {
            repo.get("full_name")
                .and_then(Value::as_str)
                .map(|fname| format!("https://github.com/{fname}"))
        })?;
    match subject.get("type").and_then(Value::as_str)? {
        // Issue/PullRequest map to their web page via the subject number.
        ty @ ("Issue" | "PullRequest") => {
            let path = if ty == "Issue" { "issues" } else { "pull" };
            // Number is the last path segment of the subject API URL.
            let number = subject
                .get("url")
                .and_then(Value::as_str)?
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))?;
            Some(format!("{base}/{path}/{number}"))
        }
        // CI (CheckSuite) notifications carry no subject number or browser URL,
        // so link to the repository's Actions page where the failed run is listed.
        "CheckSuite" => Some(format!("{base}/actions")),
        // Other subject types (Discussion, Release, …) have no reliable link.
        _ => None,
    }
}

/// Workflow name from a CheckSuite title, phrased "<workflow> workflow run
/// <status> for <branch> branch". Returns the leading workflow name, or `None`
/// when the title lacks the "workflow run" marker.
fn check_suite_workflow(title: &str) -> Option<&str> {
    let idx = title.find(" workflow run")?;
    let wf = title[..idx].trim();
    (!wf.is_empty()).then_some(wf)
}

/// Branch from a CheckSuite title (the token between "for" and the trailing
/// "branch"). Returns the branch (`main`, `feature/x`, …), or `None`.
fn check_suite_branch(title: &str) -> Option<String> {
    let after = title.split(" for ").nth(1)?.trim();
    let branch = after.strip_suffix(" branch").unwrap_or(after).trim();
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Browser URL of the workflow run a CheckSuite notification refers to. Matches
/// `runs` (a repository's `workflow_runs`) by the workflow name and branch parsed
/// from `title`, then picks the run whose `updated_at` is closest to the
/// notification's `notif_updated`. Returns `None` when nothing matches.
fn match_run_url(runs: &[Value], title: &str, notif_updated: &str) -> Option<String> {
    let workflow = check_suite_workflow(title)?;
    let branch = check_suite_branch(title);
    let target = parse_ts(notif_updated);
    runs.iter()
        .filter(|r| r.get("name").and_then(Value::as_str) == Some(workflow))
        .filter(|r| match &branch {
            Some(b) => r.get("head_branch").and_then(Value::as_str) == Some(b.as_str()),
            None => true,
        })
        .min_by_key(|r| {
            let run_ts = r
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(parse_ts);
            match (target, run_ts) {
                (Some(t), Some(u)) => (t - u).num_seconds().abs(),
                _ => i64::MAX,
            }
        })
        .and_then(|r| {
            r.get("html_url")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// Parse an RFC 3339 timestamp, or `None` when it does not parse.
fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok()
}

fn parse_tasks(
    raw: &[u8],
    kind: TaskKind,
    reason: entities::InboxReason,
) -> Result<Vec<entities::Task>, Error> {
    let arr: Vec<Value> =
        serde_json::from_slice(raw).map_err(|e| Error::Api(format!("parse gh output: {e}")))?;
    Ok(arr.iter().map(|v| task_from_gh(v, kind, reason)).collect())
}

/// Drop duplicate tasks that surfaced from more than one search, keeping the
/// first occurrence. Keyed by the display id (`owner/repo#N`), falling back to
/// `url` when it is empty. Ordering matters: the searches are run most-specific
/// reason first (review-requested before involves), so the kept copy carries the
/// most useful "why it's in my list" reason.
fn dedup_by_id(tasks: Vec<entities::Task>) -> Vec<entities::Task> {
    let mut seen = std::collections::HashSet::new();
    tasks
        .into_iter()
        .filter(|t| {
            let key = if t.id.display.is_empty() {
                t.url.clone().unwrap_or_default()
            } else {
                t.id.display.clone()
            };
            seen.insert(key)
        })
        .collect()
}

/// Arguments for one `gh search` invocation: the shared `--state open` and
/// `--json` tail with the caller's filter (`--involves @me`, `--owner <login>`,
/// `--review-requested @me`) spliced in.
fn search_args(what: &str, filter: &[&str], fields: &str) -> Vec<String> {
    let mut args = vec!["search".to_owned(), what.to_owned()];
    args.extend(filter.iter().map(|s| (*s).to_owned()));
    args.extend([
        "--state".to_owned(),
        "open".to_owned(),
        "--json".to_owned(),
        fields.to_owned(),
    ]);
    args
}

/// Projection applied to the Actions API response, keeping only the four fields
/// [`match_run_url`] reads. A hundred full run objects are on the order of a
/// megabyte per repository; the four fields are a few kilobytes.
const RUNS_JQ: &str = "[.workflow_runs[] | {name, head_branch, updated_at, html_url}]";

/// Arguments reading one repository's recent workflow runs, already projected
/// down to the fields the matcher needs.
fn runs_args(repo: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        format!("repos/{repo}/actions/runs?per_page={GITHUB_MAX_PER_PAGE}"),
        "--jq".to_owned(),
        RUNS_JQ.to_owned(),
    ]
}

/// Parse the projected run list, or `None` when the output is not the expected
/// array (kept non-fatal: the notification keeps its Actions-page link).
fn parse_runs(raw: &[u8]) -> Option<Vec<Value>> {
    serde_json::from_slice::<Vec<Value>>(raw).ok()
}

/// Login per account, filled on first lookup and kept for the life of the
/// process (see [`GithubBackend::my_login`]).
fn login_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `gh`'s own token per host, read once and reused for the life of the process.
/// The inner `Option` records a failed read (not signed in, or `gh` absent) so it
/// is not retried on every call.
#[allow(clippy::type_complexity)]
fn token_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The token `gh` would use for `host`, for passing down to child invocations as
/// `GH_TOKEN`. Read at most once per host: the read itself spawns `gh auth token`,
/// which hits the keyring exactly the once this is meant to avoid repeating.
/// `None` when `gh` cannot produce one, in which case children fall back to their
/// own keyring lookup as before.
fn cached_token(host: &str) -> Option<String> {
    let key = if host.is_empty() { "github.com" } else { host };
    let mut cache = token_cache().lock().expect("token cache poisoned");
    if let Some(known) = cache.get(key) {
        return known.clone();
    }
    let token = read_gh_token(host);
    cache.insert(key.to_owned(), token.clone());
    token
}

/// Spawn `gh auth token` directly rather than through [`GhCli::run`], which would
/// ask for the very token being read.
fn read_gh_token(host: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("gh");
    cmd.args(["auth", "token"]);
    if !host.is_empty() {
        cmd.env("GH_HOST", host);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// One repository's workflow runs as last read, with the instant they were read.
struct CachedRuns {
    fetched_at: chrono::DateTime<chrono::Utc>,
    runs: Vec<Value>,
}

/// Workflow runs per `(account, repository)`, refilled by
/// [`GithubBackend::recent_runs_for`].
fn runs_cache() -> &'static Mutex<HashMap<(String, String), CachedRuns>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), CachedRuns>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Upper bound on how long a cached run list is reused, regardless of the
/// freshness check below. Bounds staleness of everything the check cannot see
/// (a run renamed or deleted, say) without making the cache useless between
/// polls.
const RUNS_CACHE_TTL_SECS: i64 = 600;

/// Whether a cached run list can answer for a notification updated at
/// `notif_updated`. Two conditions: the snapshot is younger than the TTL, and it
/// was taken *after* the notification was last updated — in which case the run
/// the notification refers to already existed when the snapshot was read, so
/// refetching cannot produce a different match. A notification timestamp that
/// does not parse is treated as unusable and forces a refetch.
fn runs_cache_is_fresh(
    fetched_at: chrono::DateTime<chrono::Utc>,
    notif_updated: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if (now - fetched_at).num_seconds() >= RUNS_CACHE_TTL_SECS {
        return false;
    }
    match notif_updated.and_then(parse_ts) {
        Some(updated) => updated.with_timezone(&chrono::Utc) <= fetched_at,
        None => false,
    }
}

/// GitHub backend over a [`GhRunner`]: reads tasks/notifications and marks
/// notifications read.
pub struct GithubBackend<R: GhRunner> {
    runner: R,
}

impl<R: GhRunner> GithubBackend<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// The authenticated user's login, for owner-scoped searches (`gh search`
    /// takes a concrete login for `--owner`, not `@me`). Cached per account for
    /// the life of the process: the login cannot change without re-authenticating
    /// `gh`, and the lookup is a network round-trip on every poll otherwise. A
    /// runner without a [`GhRunner::cache_key`] (every fake) always asks.
    fn my_login(&self) -> Result<String, Error> {
        let key = self.runner.cache_key().map(str::to_owned);
        if let Some(k) = key.as_deref() {
            let hit = login_cache()
                .lock()
                .expect("login cache poisoned")
                .get(k)
                .cloned();
            if let Some(login) = hit {
                return Ok(login);
            }
        }
        let raw = self.runner.run(&["api", "user", "--jq", ".login"])?;
        let login = String::from_utf8_lossy(&raw).trim().to_string();
        if let Some(k) = key {
            login_cache()
                .lock()
                .expect("login cache poisoned")
                .insert(k, login.clone());
        }
        Ok(login)
    }

    /// Everything on GitHub that needs my attention, aggregated and de-duplicated:
    /// issues/PRs I'm involved in (author, assignee, mention, comment), PRs whose
    /// review is requested from me, and everything open in my own repositories.
    /// The same item surfacing in several searches is collapsed by `id`.
    pub fn list_my_tasks(&self) -> Result<Vec<entities::Task>, Error> {
        use entities::InboxReason;
        const ISSUE_FIELDS: &str =
            "number,title,state,repository,assignees,createdAt,updatedAt,url";
        const PR_FIELDS: &str =
            "number,title,state,repository,assignees,createdAt,updatedAt,url,isDraft";
        let login = self.my_login()?;

        // The five searches are independent, so they go out as one batch. Their
        // order still decides priority: `run_batch` answers in input order, and
        // the more specific search precedes the broader one so its `reason` wins
        // when dedup collapses an item that surfaced in several searches.
        let searches = [
            (
                search_args("issues", &["--involves", "@me"], ISSUE_FIELDS),
                TaskKind::Issue,
                InboxReason::Involved,
            ),
            (
                search_args("issues", &["--owner", &login], ISSUE_FIELDS),
                TaskKind::Issue,
                InboxReason::Own,
            ),
            (
                search_args("prs", &["--review-requested", "@me"], PR_FIELDS),
                TaskKind::PullRequest,
                InboxReason::ReviewRequested,
            ),
            (
                search_args("prs", &["--involves", "@me"], PR_FIELDS),
                TaskKind::PullRequest,
                InboxReason::Involved,
            ),
            (
                search_args("prs", &["--owner", &login], PR_FIELDS),
                TaskKind::PullRequest,
                InboxReason::Own,
            ),
        ];
        let calls: Vec<Vec<String>> = searches.iter().map(|(args, ..)| args.clone()).collect();
        let mut out = Vec::new();
        for (raw, (_, kind, reason)) in self.runner.run_batch(&calls).into_iter().zip(searches) {
            out.extend(parse_tasks(&raw?, kind, reason)?);
        }
        // Mark tasks in repositories the user owns (repo owner == login), so the
        // client can offer a "My repos" vs "All" scope. Derived from the project
        // "owner/repo", not from `reason` (which encodes display priority).
        for t in &mut out {
            t.mine = t.project.as_deref().and_then(|p| p.split('/').next()) == Some(login.as_str());
        }
        Ok(dedup_by_id(out))
    }

    /// My GitHub notifications, normalized. Fetches read ones too (`all=true`) so
    /// the dashboard can triage handled from pending; each item carries a `read`
    /// flag ([`notification_from_gh`]) for the client to filter on.
    pub fn list_notifications(&self) -> Result<Vec<entities::Notification>, Error> {
        let mut items = self.notifications_plain()?;
        self.link_check_suite_runs(&mut items);
        Ok(items)
    }

    /// The same list without resolving CI links. Callers that only need the items
    /// themselves (counting unread, say) take this: resolving costs one Actions
    /// call per repository with a CI notification and buys nothing when no link is
    /// displayed.
    fn notifications_plain(&self) -> Result<Vec<entities::Notification>, Error> {
        let raw = self.runner.run(&["api", "notifications?all=true"])?;
        let arr: Vec<Value> = serde_json::from_slice(&raw)
            .map_err(|e| Error::Api(format!("parse gh output: {e}")))?;
        Ok(arr.iter().map(notification_from_gh).collect())
    }

    /// Upgrade each CI (CheckSuite) notification's link from the repository Actions
    /// page to the specific workflow run it refers to. The notification carries no
    /// run id, so the run is matched by the workflow name and branch parsed from
    /// the title and the run whose `updated_at` is closest to the notification's.
    /// Best-effort: a repository whose runs cannot be fetched keeps the
    /// Actions-page fallback, and a notification with no confident match is left
    /// unchanged.
    fn link_check_suite_runs(&self, items: &mut [entities::Notification]) {
        let is_ci = |i: &entities::Notification| i.kind == entities::NotifKind::CheckSuite;
        // Per repository, the latest CI notification timestamp it has to answer
        // for. A cached run list is reusable only if it was read after that
        // instant (see [`runs_cache_is_fresh`]).
        let mut newest: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for item in items.iter().filter(|i| is_ci(i)) {
            let Some(repo) = item.project.clone() else {
                continue;
            };
            let updated = item.updated_at.clone().unwrap_or_default();
            newest
                .entry(repo)
                .and_modify(|cur| {
                    if updated > *cur {
                        cur.clone_from(&updated);
                    }
                })
                .or_insert(updated);
        }
        if newest.is_empty() {
            return;
        }
        let runs_by_repo = self.recent_runs_for(&newest);
        for item in items.iter_mut() {
            if !is_ci(item) {
                continue;
            }
            let Some(runs) = item
                .project
                .as_deref()
                .and_then(|repo| runs_by_repo.get(repo))
            else {
                continue;
            };
            let updated = item.updated_at.as_deref().unwrap_or("");
            if let Some(url) = match_run_url(runs, &item.title, updated) {
                item.url = Some(url);
            }
        }
    }

    /// Recent workflow runs for each repository in `newest` (repository → latest
    /// CI notification timestamp). Repositories whose cached list is still valid
    /// are served from the cache; the rest are fetched in one batch, so a wide
    /// inbox costs one wave of concurrent calls instead of one round-trip per
    /// repository. A repository whose call or parse fails is simply absent from
    /// the result, leaving its notifications on the Actions-page fallback.
    fn recent_runs_for(
        &self,
        newest: &std::collections::BTreeMap<String, String>,
    ) -> HashMap<String, Vec<Value>> {
        let account = self.runner.cache_key().map(str::to_owned);
        let now = chrono::Utc::now();
        let mut out: HashMap<String, Vec<Value>> = HashMap::new();
        let mut stale: Vec<String> = Vec::new();
        for (repo, notif_updated) in newest {
            let hit = account.as_ref().and_then(|acct| {
                let cache = runs_cache().lock().expect("runs cache poisoned");
                cache
                    .get(&(acct.clone(), repo.clone()))
                    .filter(|e| runs_cache_is_fresh(e.fetched_at, Some(notif_updated), now))
                    .map(|e| e.runs.clone())
            });
            match hit {
                Some(runs) => {
                    out.insert(repo.clone(), runs);
                }
                None => stale.push(repo.clone()),
            }
        }
        if stale.is_empty() {
            return out;
        }
        let calls: Vec<Vec<String>> = stale.iter().map(|r| runs_args(r)).collect();
        for (repo, raw) in stale.into_iter().zip(self.runner.run_batch(&calls)) {
            let Some(runs) = raw.ok().and_then(|b| parse_runs(&b)) else {
                continue;
            };
            if let Some(acct) = account.clone() {
                runs_cache().lock().expect("runs cache poisoned").insert(
                    (acct, repo.clone()),
                    CachedRuns {
                        fetched_at: now,
                        runs: runs.clone(),
                    },
                );
            }
            out.insert(repo, runs);
        }
        out
    }

    /// Mark one notification thread as read (`PATCH /notifications/threads/{id}`).
    /// GitHub's notification list is unread-only, so a thread marked read simply
    /// drops from the next poll; there is no "mark unread" over the REST API.
    pub fn mark_notification_read(&self, id: i64) -> Result<(), Error> {
        let path = format!("notifications/threads/{id}");
        self.runner.run(&["api", "-X", "PATCH", &path])?;
        Ok(())
    }

    /// Mark every notification as read (`PUT /notifications`). Returns the number
    /// that were unread before the call, counted from the current list (GitHub's
    /// endpoint itself reports no count).
    pub fn mark_all_notifications_read(&self) -> Result<u64, Error> {
        let count = self
            .notifications_plain()?
            .iter()
            .filter(|n| !n.read)
            .count() as u64;
        self.runner.run(&["api", "-X", "PUT", "notifications"])?;
        Ok(count)
    }
}

/// Test-only fake `gh` runner, shared with the `backend` facade tests.
#[cfg(test)]
pub mod tests_support {
    use super::{Error, GhRunner};

    /// Fake runner returning canned fixtures keyed by the leading args.
    pub struct FakeGh {
        issues: Vec<u8>,
        prs: Vec<u8>,
        notifications: Vec<u8>,
    }

    impl FakeGh {
        pub fn new(issues: Vec<u8>, prs: Vec<u8>, notifications: Vec<u8>) -> Self {
            Self {
                issues,
                prs,
                notifications,
            }
        }
    }

    impl GhRunner for FakeGh {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
            match (args.first().copied(), args.get(1).copied()) {
                (Some("search"), Some("issues")) => Ok(self.issues.clone()),
                (Some("search"), Some("prs")) => Ok(self.prs.clone()),
                // `gh api user` (login lookup) vs `gh api notifications[?all=true]`.
                (Some("api"), Some("user")) => Ok(b"testuser".to_vec()),
                (Some("api"), Some(p)) if p.starts_with("notifications") => {
                    Ok(self.notifications.clone())
                }
                _ => Err(Error::Api(format!("unexpected gh args: {args:?}"))),
            }
        }
    }

    use std::cell::RefCell;
    use std::rc::Rc;

    /// Recording fake runner: returns the given notifications for
    /// `api notifications`, empty for any write, and records each invocation's
    /// joined args so a test can assert the exact endpoint and method.
    pub struct RecordGh {
        notifications: Vec<u8>,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl RecordGh {
        /// Returns the runner and a shared handle to inspect recorded calls after
        /// the runner has been moved into a backend.
        pub fn new(notifications: Vec<u8>) -> (Self, Rc<RefCell<Vec<String>>>) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    notifications,
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    impl GhRunner for RecordGh {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
            self.calls.borrow_mut().push(args.join(" "));
            match (args.first().copied(), args.get(1).copied()) {
                (Some("api"), Some(p)) if p.starts_with("notifications") => {
                    Ok(self.notifications.clone())
                }
                _ => Ok(Vec::new()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{FakeGh, RecordGh};
    use super::*;
    use serde_json::json;

    #[test]
    fn mark_notification_read_patches_the_thread() {
        let (gh, calls) = RecordGh::new(b"[]".to_vec());
        GithubBackend::new(gh).mark_notification_read(42).unwrap();
        assert_eq!(
            calls.borrow().as_slice(),
            &["api -X PATCH notifications/threads/42"]
        );
    }

    #[test]
    fn mark_all_read_counts_unread_then_puts() {
        // The list carries read items too (`all=true`); only the unread ones are
        // counted, so an already-read item must not inflate the reported number.
        let notifs = json!([
            {"id":"1","unread":true,"subject":{"title":"A"}},
            {"id":"2","unread":true,"subject":{"title":"B"}},
            {"id":"3","unread":false,"subject":{"title":"already read"}}
        ])
        .to_string()
        .into_bytes();
        let (gh, calls) = RecordGh::new(notifs);
        let count = GithubBackend::new(gh)
            .mark_all_notifications_read()
            .unwrap();
        assert_eq!(count, 2);
        // The list is read first (to count), then the mark-all PUT is issued.
        assert_eq!(
            calls.borrow().as_slice(),
            &["api notifications?all=true", "api -X PUT notifications"]
        );
    }

    #[test]
    fn list_notifications_requests_read_ones_too() {
        let (gh, calls) = RecordGh::new(b"[]".to_vec());
        GithubBackend::new(gh).list_notifications().unwrap();
        assert_eq!(calls.borrow().as_slice(), &["api notifications?all=true"]);
    }

    #[test]
    fn notification_from_gh_sets_read_flag_from_unread() {
        let read = notification_from_gh(&json!({
            "id": "1", "unread": false, "subject": {"title": "done"}
        }));
        assert!(read.read);
        let unread = notification_from_gh(&json!({
            "id": "2", "unread": true, "subject": {"title": "pending"}
        }));
        assert!(!unread.read);
        // A missing `unread` flag is treated as unread (read == false).
        let absent = notification_from_gh(&json!({
            "id": "3", "subject": {"title": "legacy"}
        }));
        assert!(!absent.read);
    }

    /// Fake runner whose `--version` / `auth status` outcomes are configurable,
    /// to classify [`gh_status`] without the real binary.
    struct StatusGh {
        installed: bool,
        authed: bool,
    }

    impl GhRunner for StatusGh {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
            match args.first().copied() {
                Some("--version") if self.installed => Ok(Vec::new()),
                Some("--version") => Err(Error::Io("spawn gh: not found".into())),
                Some("auth") if self.authed => Ok(Vec::new()),
                Some("auth") => Err(Error::Api("not logged in".into())),
                _ => Err(Error::Api(format!("unexpected gh args: {args:?}"))),
            }
        }
    }

    #[test]
    fn gh_status_classifies_missing_unauth_ready() {
        assert_eq!(
            gh_status(&StatusGh {
                installed: false,
                authed: false
            }),
            GhStatus::Missing
        );
        assert_eq!(
            gh_status(&StatusGh {
                installed: true,
                authed: false
            }),
            GhStatus::Unauthenticated
        );
        assert_eq!(
            gh_status(&StatusGh {
                installed: true,
                authed: true
            }),
            GhStatus::Ready
        );
    }

    /// Fake runner that answers `gh api user --jq .login` with a fixed login.
    struct AccountGh {
        login: &'static str,
    }

    impl GhRunner for AccountGh {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
            match args {
                ["api", "user", "--jq", ".login"] => Ok(self.login.as_bytes().to_vec()),
                _ => Err(Error::Api(format!("unexpected gh args: {args:?}"))),
            }
        }
    }

    #[test]
    fn gh_account_reads_login_and_defaults_host() {
        // Empty host resolves to github.com; login trimmed from `gh api user`.
        let acc = gh_account(&AccountGh { login: "octocat\n" }, "").unwrap();
        assert_eq!(acc.login, "octocat");
        assert_eq!(acc.host, "github.com");
        // An explicit enterprise host is echoed verbatim.
        let ent = gh_account(&AccountGh { login: "worker" }, "ghe.example").unwrap();
        assert_eq!(ent.host, "ghe.example");
    }

    #[test]
    fn notification_from_gh_browser_url_for_pr_and_fallback() {
        // PullRequest maps to the `/pull/N` web path (not the API `/pulls/N`).
        let pr = json!({
            "subject": {"title": "PR", "type": "PullRequest", "url": "https://api.github.com/repos/acme/app/pulls/7"},
            "repository": {"full_name": "acme/app", "html_url": "https://github.com/acme/app"}
        });
        assert_eq!(
            notification_from_gh(&pr).url.as_deref(),
            Some("https://github.com/acme/app/pull/7")
        );
        // No html_url on the repository: fall back to github.com/<full_name>.
        let fallback = json!({
            "subject": {"title": "I", "type": "Issue", "url": "https://api.github.com/repos/acme/app/issues/3"},
            "repository": {"full_name": "acme/app"}
        });
        assert_eq!(
            notification_from_gh(&fallback).url.as_deref(),
            Some("https://github.com/acme/app/issues/3")
        );
        // Unsupported subject type: no link.
        let disc = json!({
            "subject": {"title": "D", "type": "Discussion", "url": "https://api.github.com/repos/acme/app/discussions/1"},
            "repository": {"full_name": "acme/app"}
        });
        assert_eq!(notification_from_gh(&disc).url, None);
    }

    #[test]
    fn notification_from_gh_check_suite_links_to_actions_page() {
        // CI (CheckSuite) notifications carry no subject number/url; they link to
        // the repository's Actions page (later upgraded to the specific run).
        let ci = json!({
            "subject": {"title": "CI workflow run failed", "type": "CheckSuite", "url": null},
            "repository": {"full_name": "acme/app", "html_url": "https://github.com/acme/app"}
        });
        assert_eq!(
            notification_from_gh(&ci).url.as_deref(),
            Some("https://github.com/acme/app/actions")
        );
    }

    #[test]
    fn check_suite_outcome_classifies_by_title() {
        assert_eq!(
            check_suite_outcome("CI workflow run failed for main branch"),
            "failure"
        );
        assert_eq!(
            check_suite_outcome("CI workflow run succeeded for main branch"),
            "success"
        );
        assert_eq!(check_suite_outcome("All checks passed"), "success");
        assert_eq!(check_suite_outcome("CI workflow run cancelled"), "neutral");
        assert_eq!(check_suite_outcome(""), "neutral");
    }

    #[test]
    fn task_from_gh_maps_to_the_typed_entity() {
        let v = json!({
            "number": 7, "title": "Fix gearbox", "state": "open",
            "repository": {"nameWithOwner": "acme/widgets"},
            "assignees": [{"login": "dana"}, {"login": "robin"}],
            "createdAt": "2026-07-01T09:00:00Z", "updatedAt": "2026-07-10T12:00:00Z",
            "url": "https://example.test/acme/widgets/pull/7"
        });
        let t = task_from_gh(
            &v,
            TaskKind::PullRequest,
            entities::InboxReason::ReviewRequested,
        );
        assert_eq!(t.id.display, "acme/widgets#7");
        assert_eq!(t.id.raw, "7");
        assert_eq!(t.kind, entities::TaskKind::PullRequest);
        assert_eq!(t.reason, entities::InboxReason::ReviewRequested);
        assert_eq!(t.title, "Fix gearbox");
        assert_eq!(t.status.as_deref(), Some("open"));
        assert_eq!(t.status_category, entities::StatusCategory::Open);
        assert_eq!(t.project.as_deref(), Some("acme/widgets"));
        assert_eq!(t.assignee.as_deref(), Some("dana, robin"));
        assert_eq!(
            t.url.as_deref(),
            Some("https://example.test/acme/widgets/pull/7")
        );
    }

    #[test]
    fn notification_from_gh_maps_check_suite_with_outcome_and_browser_url() {
        let v = json!({
            "id": "42", "reason": "ci_activity", "unread": true,
            "subject": {"title": "CI workflow run failed for main branch", "type": "CheckSuite"},
            "repository": {"full_name": "acme/widgets"},
            "updated_at": "2026-07-10T12:00:00Z"
        });
        let n = notification_from_gh(&v);
        assert_eq!(n.id, "42");
        assert_eq!(n.reason, "ci_activity");
        assert_eq!(n.kind, entities::NotifKind::CheckSuite);
        assert_eq!(n.outcome, Some(entities::CiOutcome::Failure));
        assert!(!n.read);
        assert_eq!(n.wp_id, None);
        // Issue notification: browser url derived from the subject number, read flag set.
        let issue = json!({
            "id": "9", "reason": "mention", "unread": false,
            "subject": {"title": "Ping", "type": "Issue",
                "url": "https://api.github.com/repos/acme/widgets/issues/3"},
            "repository": {"full_name": "acme/widgets"}
        });
        let n2 = notification_from_gh(&issue);
        assert_eq!(n2.kind, entities::NotifKind::Issue);
        assert!(n2.read);
        assert_eq!(n2.outcome, None);
        assert_eq!(
            n2.url.as_deref(),
            Some("https://github.com/acme/widgets/issues/3")
        );
    }

    #[test]
    fn list_my_tasks_merges_issues_and_prs() {
        let fake = FakeGh::new(
            json!([{
                "number": 1, "title": "I1", "state": "open",
                "repository": {"nameWithOwner": "acme/app"}, "assignees": [{"login": "me"}]
            }])
            .to_string()
            .into_bytes(),
            json!([{
                "number": 2, "title": "P2", "state": "open",
                "repository": {"nameWithOwner": "acme/app"}, "assignees": []
            }])
            .to_string()
            .into_bytes(),
            b"[]".to_vec(),
        );
        // The same issue/PR fixture is returned by several searches (involves,
        // owner, review-requested); dedup_by_id collapses each to one entry.
        let out = GithubBackend::new(fake).list_my_tasks().unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, entities::TaskKind::Issue);
        assert_eq!(out[0].id.display, "acme/app#1");
        assert_eq!(out[1].kind, entities::TaskKind::PullRequest);
        assert_eq!(out[1].id.display, "acme/app#2");
        // The PR surfaces first from the review-requested search, so its reason wins.
        assert_eq!(out[1].reason, entities::InboxReason::ReviewRequested);
        // "acme/app" is not owned by the login ("testuser"), so not "mine".
        assert!(!out[0].mine);
        assert!(!out[1].mine);
    }

    #[test]
    fn list_my_tasks_marks_tasks_in_own_repos() {
        // One issue in the login's own repo, one in someone else's.
        let fake = FakeGh::new(
            json!([
                {"number": 1, "title": "mine", "state": "open",
                 "repository": {"nameWithOwner": "testuser/app"}, "assignees": []},
                {"number": 2, "title": "theirs", "state": "open",
                 "repository": {"nameWithOwner": "acme/app"}, "assignees": []}
            ])
            .to_string()
            .into_bytes(),
            b"[]".to_vec(),
            b"[]".to_vec(),
        );
        let out = GithubBackend::new(fake).list_my_tasks().unwrap();
        let mine = out
            .iter()
            .find(|t| t.id.display == "testuser/app#1")
            .unwrap();
        let theirs = out.iter().find(|t| t.id.display == "acme/app#2").unwrap();
        assert!(mine.mine, "own-repo task must be marked mine");
        assert!(!theirs.mine, "other-repo task must not be marked mine");
    }

    /// Build a minimal typed task with the given display id and reason.
    fn task(display: &str, reason: entities::InboxReason) -> entities::Task {
        entities::Task {
            id: entities::TaskId {
                display: display.into(),
                raw: display.into(),
            },
            kind: entities::TaskKind::Issue,
            reason,
            title: String::new(),
            url: None,
            status: None,
            status_category: entities::StatusCategory::Unknown,
            project: None,
            mine: false,
            assignee: None,
            author: None,
            created_at: None,
            updated_at: None,
            due_date: None,
            priority: None,
            labels: Vec::new(),
            custom_fields: Vec::new(),
        }
    }

    #[test]
    fn dedup_by_id_keeps_first_occurrence_and_order() {
        let a1 = task("acme/app#1", entities::InboxReason::ReviewRequested);
        let a1_dup = task("acme/app#1", entities::InboxReason::Involved);
        let b2 = task("acme/app#2", entities::InboxReason::Own);
        let out = dedup_by_id(vec![a1, b2, a1_dup]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id.display, "acme/app#1");
        // The first occurrence (ReviewRequested) is kept over the later duplicate.
        assert_eq!(out[0].reason, entities::InboxReason::ReviewRequested);
        assert_eq!(out[1].id.display, "acme/app#2");
    }

    #[test]
    fn parses_workflow_and_branch_from_check_suite_title() {
        let t = "CI workflow run failed for deps/keyring-core branch";
        assert_eq!(check_suite_workflow(t), Some("CI"));
        assert_eq!(check_suite_branch(t), Some("deps/keyring-core".to_string()));
        // A title without the marker yields no workflow / no branch.
        assert_eq!(check_suite_workflow("Deploy done"), None);
        assert_eq!(check_suite_branch("no branch marker here"), None);
    }

    #[test]
    fn match_run_url_picks_workflow_branch_and_nearest_time() {
        let runs = json!([
            {"name":"CI","head_branch":"feat","updated_at":"2026-07-15T11:21:20Z",
             "html_url":"https://github.com/acme/app/actions/runs/999"},
            {"name":"CI","head_branch":"master","updated_at":"2026-07-15T11:21:19Z",
             "html_url":"https://github.com/acme/app/actions/runs/111"},
            {"name":"Audit","head_branch":"feat","updated_at":"2026-07-15T11:21:18Z",
             "html_url":"https://github.com/acme/app/actions/runs/222"},
            {"name":"CI","head_branch":"feat","updated_at":"2026-07-10T00:00:00Z",
             "html_url":"https://github.com/acme/app/actions/runs/333"}
        ]);
        let runs = runs.as_array().unwrap();
        // Workflow "CI" + branch "feat", nearest to 11:21:18 → run 999 (not the
        // wrong branch 111, wrong workflow 222, or the far-older 333).
        assert_eq!(
            match_run_url(
                runs,
                "CI workflow run failed for feat branch",
                "2026-07-15T11:21:18Z"
            ),
            Some("https://github.com/acme/app/actions/runs/999".to_string())
        );
        // No run for that workflow → no match.
        assert_eq!(
            match_run_url(
                runs,
                "Release workflow run failed for feat branch",
                "2026-07-15T11:21:18Z"
            ),
            None
        );
    }

    /// Fake runner serving both the notifications inbox and a repository's runs.
    struct CiGh {
        notifs: Vec<u8>,
        runs: Result<Vec<u8>, ()>,
    }
    impl GhRunner for CiGh {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
            match (args.first().copied(), args.get(1).copied()) {
                (Some("api"), Some(p)) if p.starts_with("notifications") => Ok(self.notifs.clone()),
                (Some("api"), Some(p)) if p.contains("/actions/runs") => self
                    .runs
                    .clone()
                    .map_err(|()| Error::Api("runs unavailable".into())),
                _ => Err(Error::Api(format!("unexpected gh args: {args:?}"))),
            }
        }
    }

    fn ci_notification() -> Vec<u8> {
        json!([{
            "id":"1","reason":"ci_activity",
            "subject":{"title":"CI workflow run failed for deps/keyring-core branch",
                       "type":"CheckSuite","url":null},
            "repository":{"full_name":"acme/app","html_url":"https://github.com/acme/app"},
            "updated_at":"2026-07-15T11:21:18Z"
        }])
        .to_string()
        .into_bytes()
    }

    #[test]
    fn list_notifications_links_check_suite_to_specific_run() {
        // Shaped like the `--jq` projection the backend asks for: a flat array of
        // the four fields the matcher reads, not the raw `workflow_runs` envelope.
        let runs = json!([
            {"name":"CI","head_branch":"deps/keyring-core","updated_at":"2026-07-15T11:21:20Z",
             "html_url":"https://github.com/acme/app/actions/runs/999"},
            {"name":"CI","head_branch":"master","updated_at":"2026-07-15T11:21:19Z",
             "html_url":"https://github.com/acme/app/actions/runs/111"}
        ])
        .to_string()
        .into_bytes();
        let gh = CiGh {
            notifs: ci_notification(),
            runs: Ok(runs),
        };
        let out = GithubBackend::new(gh).list_notifications().unwrap();
        assert_eq!(
            out[0].url.as_deref(),
            Some("https://github.com/acme/app/actions/runs/999")
        );
    }

    #[test]
    fn check_suite_keeps_actions_fallback_when_runs_unavailable() {
        let gh = CiGh {
            notifs: ci_notification(),
            runs: Err(()),
        };
        let out = GithubBackend::new(gh).list_notifications().unwrap();
        // Runs could not be fetched → the link stays the repository Actions page.
        assert_eq!(
            out[0].url.as_deref(),
            Some("https://github.com/acme/app/actions")
        );
    }

    #[test]
    fn runs_request_projects_to_the_matched_fields() {
        let args = runs_args("acme/app");
        assert_eq!(
            args,
            vec![
                "api".to_owned(),
                "repos/acme/app/actions/runs?per_page=100".to_owned(),
                "--jq".to_owned(),
                "[.workflow_runs[] | {name, head_branch, updated_at, html_url}]".to_owned(),
            ]
        );
    }

    #[test]
    fn runs_cache_reuse_requires_snapshot_newer_than_the_notification() {
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let fetched = at("2026-07-15T12:00:00Z");
        let now = at("2026-07-15T12:01:00Z");
        // Notification predates the snapshot → the run it names was already in it.
        assert!(runs_cache_is_fresh(
            fetched,
            Some("2026-07-15T11:59:59Z"),
            now
        ));
        // Notification is newer than the snapshot → its run may be missing there.
        assert!(!runs_cache_is_fresh(
            fetched,
            Some("2026-07-15T12:00:01Z"),
            now
        ));
        // Same instant is still covered by the snapshot.
        assert!(runs_cache_is_fresh(
            fetched,
            Some("2026-07-15T12:00:00Z"),
            now
        ));
        // Past the TTL nothing is reused, however old the notification is.
        assert!(!runs_cache_is_fresh(
            fetched,
            Some("2026-07-15T10:00:00Z"),
            at("2026-07-15T12:10:00Z")
        ));
        // An unusable timestamp cannot prove the snapshot covers the run.
        assert!(!runs_cache_is_fresh(fetched, Some("not a date"), now));
        assert!(!runs_cache_is_fresh(fetched, None, now));
    }

    /// Runner counting how often each endpoint was hit, reporting a caller-chosen
    /// cache key so a test can exercise the cached and uncached paths.
    struct CountingGh {
        key: Option<&'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl CountingGh {
        fn new(key: Option<&'static str>) -> Self {
            Self {
                key,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn count_of(&self, needle: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.contains(needle))
                .count()
        }
    }

    impl GhRunner for CountingGh {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
            self.calls.lock().unwrap().push(args.join(" "));
            match (args.first().copied(), args.get(1).copied()) {
                (Some("api"), Some("user")) => Ok(b"testuser".to_vec()),
                // Writes (`api -X PUT notifications`) answer empty.
                (Some("api"), Some("-X")) => Ok(Vec::new()),
                (Some("api"), Some(p)) if p.starts_with("notifications") => Ok(ci_notification()),
                (Some("api"), Some(p)) if p.contains("/actions/runs") => Ok(json!([
                    {"name":"CI","head_branch":"deps/keyring-core",
                     "updated_at":"2026-07-15T11:21:20Z",
                     "html_url":"https://github.com/acme/app/actions/runs/999"}
                ])
                .to_string()
                .into_bytes()),
                (Some("search"), _) => Ok(b"[]".to_vec()),
                _ => Err(Error::Api(format!("unexpected gh args: {args:?}"))),
            }
        }

        fn cache_key(&self) -> Option<&str> {
            self.key
        }
    }

    #[test]
    fn mark_all_read_counts_without_resolving_ci_links() {
        // The inbox here is a CI notification, so listing it for display would
        // fetch the repository's runs. Counting unread must not: no link is shown.
        let gh = CountingGh::new(None);
        let count = GithubBackend::new(&gh)
            .mark_all_notifications_read()
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(gh.count_of("/actions/runs"), 0);
        // Listing the same inbox for display still resolves the link.
        GithubBackend::new(&gh).list_notifications().unwrap();
        assert_eq!(gh.count_of("/actions/runs"), 1);
    }

    #[test]
    fn login_lookup_is_cached_per_account() {
        // A runner without a cache key (every fake but this one) always asks.
        let uncached = CountingGh::new(None);
        GithubBackend::new(&uncached).list_my_tasks().unwrap();
        GithubBackend::new(&uncached).list_my_tasks().unwrap();
        assert_eq!(uncached.count_of("api user"), 2);

        // With a key the login is read once and reused by later backends.
        let cached = CountingGh::new(Some("login-cache-test.example"));
        GithubBackend::new(&cached).list_my_tasks().unwrap();
        GithubBackend::new(&cached).list_my_tasks().unwrap();
        assert_eq!(cached.count_of("api user"), 1);
    }

    #[test]
    fn workflow_runs_are_reused_while_the_snapshot_still_covers_the_notification() {
        let uncached = CountingGh::new(None);
        GithubBackend::new(&uncached).list_notifications().unwrap();
        GithubBackend::new(&uncached).list_notifications().unwrap();
        assert_eq!(uncached.count_of("/actions/runs"), 2);

        // The notification is dated well in the past, so the first snapshot
        // answers for it and the second poll issues no Actions call at all.
        let cached = CountingGh::new(Some("runs-cache-test.example"));
        let first = GithubBackend::new(&cached).list_notifications().unwrap();
        let second = GithubBackend::new(&cached).list_notifications().unwrap();
        assert_eq!(cached.count_of("/actions/runs"), 1);
        // The cached pass resolves the same specific run as the fetched one.
        assert_eq!(
            second[0].url.as_deref(),
            Some("https://github.com/acme/app/actions/runs/999")
        );
        assert_eq!(first[0].url, second[0].url);
    }

    #[test]
    fn list_notifications_normalizes() {
        let fake = FakeGh::new(
            b"[]".to_vec(),
            b"[]".to_vec(),
            json!([{
                "id": "1", "reason": "assign",
                "subject": {"title": "T", "type": "PullRequest", "url": "u"},
                "repository": {"full_name": "acme/app"}, "updated_at": "2026-07-02T00:00:00Z"
            }])
            .to_string()
            .into_bytes(),
        );
        let out = GithubBackend::new(fake).list_notifications().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "T");
        assert_eq!(out[0].project.as_deref(), Some("acme/app"));
    }
}
