import { get } from "svelte/store";
import {
  listServers,
  listTasks,
  listNotifications,
  getTimelog,
  notifyItems,
  type NotifyItem,
} from "./api";
import {
  servers,
  byServer,
  summaries,
  activeServer,
  syncByServer,
  timelog,
  settings,
  unreadOf,
  freshUnread,
  toCacheEntry,
  fromCacheEntry,
  parseCacheEntry,
  type ServerState,
  type SyncPhase,
} from "./store";
import { supportsTaskDetail } from "./capabilities";
import type { Task } from "./types";
import { t, locale, plural } from "./i18n";
import { goto } from "$app/navigation";
import { openExternal } from "./external";
import type { ServerInfo, Notification } from "./types";

// Unread notification ids observed on the previous poll of each server, so a
// desktop notification fires only for ids that are new (see `freshUnread`).
const seenUnread = new Map<string, Set<string>>();
// More than this many new items at once collapse into a single summary banner
// instead of one banner per item.
const NOTIFY_COLLAPSE = 3;

/**
 * Desktop-notify the unread items that are new since the last poll of `s`. The
 * first poll only establishes a baseline (no startup burst); the settings toggle
 * suppresses banners but the baseline is still tracked so enabling it later does
 * not dump the whole backlog.
 */
function maybeNotify(s: ServerInfo, notifications: Notification[]): void {
  const prev = seenUnread.get(s.name);
  const { fresh, seen } = freshUnread(prev, notifications);
  seenUnread.set(s.name, seen);
  if (prev === undefined || fresh.length === 0) return;
  if (!get(settings).desktop_notifications) return;
  // A notification failure must never abort the poll (which also refreshes the
  // task/notification lists), so swallow any error here.
  try {
    const items = buildNotifyItems(s, fresh);
    if (items.length > 0) void notifyItems(items).catch(() => {});
  } catch (e) {
    console.error("desktop notification failed", e);
  }
}

/** Route a click on a desktop notification to the item it announced. Mirrors the
 * in-app targets: an OpenProject task opens its detail screen, a GitHub item
 * opens in the browser, a summary just focuses the server. */
function routeNotification(payload: unknown): void {
  const p = payload as Record<string, unknown> | null;
  if (!p || typeof p.kind !== "string") return;
  if (p.kind === "external" && typeof p.url === "string") {
    void openExternal(p.url);
    return;
  }
  if (typeof p.server === "string") activeServer.set(p.server);
  if (p.kind === "task" && p.server != null && p.id != null) {
    void goto(
      `/task?server=${encodeURIComponent(String(p.server))}&id=${encodeURIComponent(String(p.id))}`,
    );
  } else {
    void goto("/");
  }
}

/** Subscribe to click-through events from Linux desktop notifications. No-op in
 * a plain browser (dev-mock) where the Tauri event bus is absent. */
async function registerNotificationClick(): Promise<void> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window))
    return;
  const { listen } = await import("@tauri-apps/api/event");
  await listen("open-notification", (e) => routeNotification(e.payload));
}

/**
 * Turn fresh notifications into banner items. A click target routes the banner:
 * an OpenProject item opens its detail screen, a GitHub item opens the issue/PR
 * in the browser, anything else just focuses the server. Many at once collapse
 * into one summary that focuses the server.
 */
function buildNotifyItems(s: ServerInfo, fresh: Notification[]): NotifyItem[] {
  const label = s.display_name || s.name;
  if (fresh.length > NOTIFY_COLLAPSE) {
    const suffix = plural(get(locale), fresh.length, get(t), "notif.newCount");
    return [
      {
        title: label,
        body: `${fresh.length} ${suffix}`,
        target: { kind: "server", server: s.name },
      },
    ];
  }
  return fresh.map((n) => {
    const reason = n.reason ? `${n.reason}: ` : "";
    let target: unknown;
    if (supportsTaskDetail(s) && n.wpId != null) {
      target = { kind: "task", server: s.name, id: n.wpId };
    } else if (n.url) {
      target = { kind: "external", url: n.url };
    } else {
      target = { kind: "server", server: s.name };
    }
    return { title: label, body: reason + n.title, target };
  });
}

const timers = new Map<string, ReturnType<typeof setInterval>>();
let timelogTimer: ReturnType<typeof setInterval> | undefined;
let resumeHandler: (() => void) | undefined;
let resuming = false;
let unsubActive: (() => void) | undefined;

/** Aggregate timelog refresh interval (seconds). */
const TIMELOG_INTERVAL_SECS = 120;

/** Shortest gap between two focus/online-triggered refreshes (see
 * [`refreshOnResume`]). */
const RESUME_MIN_GAP_MS = 60_000;

/** When the last full refresh finished, for the gap above. */
let lastRefreshDoneMs = 0;

/** Load the server list, seed the active server, and start per-server polling. */
export async function startPolling(): Promise<void> {
  const list = await listServers();
  servers.set(list);
  const enabled = list.filter((s) => s.enabled);
  if (
    get(activeServer) === null ||
    !enabled.some((s) => s.name === get(activeServer))
  ) {
    const def = enabled.find((s) => s.is_default) ?? enabled[0];
    activeServer.set(def ? def.name : null);
  }
  // Show the last-known data from cache immediately, before the first poll.
  seedFromCache();
  for (const s of enabled) {
    void pollOnce(s);
    const id = setInterval(() => void pollOnce(s), s.poll_secs * 1000);
    timers.set(s.name, id);
  }
  void refreshTimelog();
  timelogTimer = setInterval(
    () => void refreshTimelog(),
    TIMELOG_INTERVAL_SECS * 1000,
  );

  // Switching servers loads the newly active one in full and evicts the
  // previously active one's arrays back to a summary, bounding resident data.
  unsubActive = activeServer.subscribe((name) => {
    if (name) void onActivate(name);
  });

  // setInterval timers are suspended while the system sleeps and resume only on
  // the next tick, so data is stale for up to one interval after wake. Refresh
  // when the window regains focus or connectivity is restored, throttled so
  // ordinary window switching does not restart the poll each time.
  resumeHandler = () => void refreshOnResume();
  window.addEventListener("focus", resumeHandler);
  window.addEventListener("online", resumeHandler);

  // Route clicks on Linux desktop notifications to the item they announced.
  void registerNotificationClick();
}

export function stopPolling(): void {
  for (const id of timers.values()) clearInterval(id);
  timers.clear();
  if (timelogTimer) clearInterval(timelogTimer);
  timelogTimer = undefined;
  if (unsubActive) {
    unsubActive();
    unsubActive = undefined;
  }
  if (resumeHandler) {
    window.removeEventListener("focus", resumeHandler);
    window.removeEventListener("online", resumeHandler);
    resumeHandler = undefined;
  }
}

/** Refresh every enabled server and the aggregate timelog at once. */
export async function refreshAll(): Promise<void> {
  if (resuming) return;
  resuming = true;
  try {
    const enabled = get(servers).filter((s) => s.enabled);
    await Promise.all([...enabled.map((s) => pollOnce(s)), refreshTimelog()]);
  } finally {
    resuming = false;
    lastRefreshDoneMs = Date.now();
  }
}

/**
 * Refresh after the window regained focus or connectivity, unless one just
 * finished. Without this every return to the window starts a full refresh, and
 * a slow backend (GitHub polls several endpoints per server) would then be
 * synchronizing more or less permanently while the window is in use.
 */
export async function refreshOnResume(): Promise<void> {
  if (Date.now() - lastRefreshDoneMs < RESUME_MIN_GAP_MS) return;
  await refreshAll();
}

/** Refresh a single server now (after a write action). */
export async function refreshServer(name: string): Promise<void> {
  const s = get(servers).find((x) => x.name === name);
  if (s) await pollOnce(s);
}

/** Refresh the aggregate timelog; keep the last value on failure. */
export async function refreshTimelog(): Promise<void> {
  try {
    timelog.set(await getTimelog());
  } catch {
    // Keep the previous value on transient errors.
  }
}

/** Load the newly activated server in full if its arrays are not resident. */
async function onActivate(name: string): Promise<void> {
  // Evict every other server's resident arrays; their summaries remain.
  byServer.update((by) => {
    const next: typeof by = {};
    if (by[name]) next[name] = by[name];
    return next;
  });
  if (!get(byServer)[name]) {
    const s = get(servers).find((x) => x.name === name);
    if (s) await pollOnce(s);
  }
}

/** Local-storage key for a server's cached first page. */
function cacheKey(name: string): string {
  return `laba:cache:${name}`;
}

/** Persist a server's first page so it can be shown instantly on next launch. */
function writeCache(
  name: string,
  entry: ReturnType<typeof toCacheEntry>,
): void {
  try {
    localStorage.setItem(cacheKey(name), JSON.stringify(entry));
  } catch {
    // Storage unavailable or over quota: the cache is best-effort.
  }
}

/** Read a server's cached first page, or null when absent/corrupt. */
function readCache(name: string) {
  try {
    return parseCacheEntry(localStorage.getItem(cacheKey(name)));
  } catch {
    return null;
  }
}

/** Update a server's sync phase, preserving its last-success timestamp. */
function setSyncPhase(
  name: string,
  phase: SyncPhase,
  lastSyncMs?: number,
): void {
  syncByServer.update((m) => {
    const prev = m[name];
    return {
      ...m,
      [name]: {
        phase,
        lastSyncMs: lastSyncMs ?? prev?.lastSyncMs ?? null,
      },
    };
  });
}

/**
 * Seed the dashboard from the on-disk cache before the first network poll, so
 * the columns and unread badges show the last-known data immediately instead of
 * an empty spinner. The active server's full arrays are made resident; every
 * cached server's unread count seeds its summary. A following poll replaces this.
 */
export function seedFromCache(): void {
  const active = get(activeServer);
  for (const s of get(servers).filter((x) => x.enabled)) {
    const entry = readCache(s.name);
    if (!entry) continue;
    summaries.update((m) =>
      s.name in m
        ? m
        : { ...m, [s.name]: { error: null, unread: entry.unread } },
    );
    setSyncPhase(s.name, "syncing", entry.savedAtMs || undefined);
    if (s.name === active && !get(byServer)[s.name]) {
      byServer.update((by) => ({ ...by, [s.name]: fromCacheEntry(entry) }));
    }
  }
}

/**
 * Refresh one server's first page. The active server keeps its full arrays and
 * page cursors resident in `byServer`; other servers retain only a summary
 * (error flag + unread count) so memory stays bounded by the viewport. On
 * failure old data and the last summary are kept and the error is recorded.
 */
async function pollOnce(s: ServerInfo): Promise<void> {
  setSyncPhase(s.name, "syncing");
  // The two halves are applied as each arrives rather than after both, so the
  // task column is not held back by the notification request (on GitHub the
  // latter also resolves CI links, which costs extra round-trips).
  const [tasksRes, notifsRes] = await Promise.allSettled([
    listTasks(s.name, 1).then((page) => {
      applyPart(s.name, {
        tasks: page.items,
        taskCursor: page.next_offset,
        error: null,
      });
      return page;
    }),
    fetchNotifications(s),
  ]);

  if (tasksRes.status === "rejected" || notifsRes.status === "rejected") {
    const message = String(
      tasksRes.status === "rejected"
        ? tasksRes.reason
        : notifsRes.status === "rejected"
          ? notifsRes.reason
          : "",
    );
    summaries.update((m) => ({
      ...m,
      [s.name]: { error: message, unread: m[s.name]?.unread ?? 0 },
    }));
    setSyncPhase(s.name, "stale");
    // Whatever half did arrive stays; only the error flag is added on top.
    applyPart(s.name, { error: message });
    return;
  }

  setSyncPhase(s.name, "idle", Date.now());
  const state: ServerState = {
    tasks: tasksRes.value.items,
    notifications: notifsRes.value.items,
    error: null,
    taskCursor: tasksRes.value.next_offset,
    notifCursor: notifsRes.value.next_offset,
  };
  writeCache(
    s.name,
    toCacheEntry(
      state,
      notifsRes.value.items.filter(unreadOf).length,
      Date.now(),
    ),
  );
}

/** Fetch a server's first page of notifications and apply it: unread summary,
 * resident array, desktop banners for what is newly unread. */
async function fetchNotifications(
  s: ServerInfo,
): Promise<Awaited<ReturnType<typeof listNotifications>>> {
  const page = await listNotifications(s.name, 1);
  const unread = page.items.filter(unreadOf).length;
  summaries.update((m) => ({ ...m, [s.name]: { error: null, unread } }));
  applyPart(s.name, {
    notifications: page.items,
    notifCursor: page.next_offset,
    error: null,
  });
  // Announce items that became unread since the last poll (all servers, not
  // just the active one).
  maybeNotify(s, page.items);
  return page;
}

/**
 * Refresh only a server's notifications. Used after writing a read state, where
 * the task list cannot have changed — reloading it too would double the work of
 * an action whose whole effect is on one column.
 */
export async function refreshNotifications(name: string): Promise<void> {
  const s = get(servers).find((x) => x.name === name);
  if (!s) return;
  setSyncPhase(name, "syncing");
  try {
    const page = await fetchNotifications(s);
    setSyncPhase(name, "idle", Date.now());
    // Keep the on-disk cache consistent with what is now on screen; the task
    // half comes from the resident state, which this refresh left untouched.
    const cur = get(byServer)[name];
    if (cur) {
      writeCache(
        name,
        toCacheEntry(cur, page.items.filter(unreadOf).length, Date.now()),
      );
    }
  } catch (e) {
    const message = String(e);
    summaries.update((m) => ({
      ...m,
      [name]: { error: message, unread: m[name]?.unread ?? 0 },
    }));
    setSyncPhase(name, "stale");
    applyPart(name, { error: message });
  }
}

/**
 * Merge one finished half of a poll into a server's resident state. The active
 * server is re-read here, after the request settled: the user may have switched
 * servers while it was in flight, and deciding by the pre-request snapshot would
 * let a stale poll leave resident arrays on a now-inactive server (whose summary
 * alone is enough).
 */
function applyPart(name: string, part: Partial<ServerState>): void {
  if (get(activeServer) !== name) {
    byServer.update((by) => {
      if (!(name in by)) return by;
      const { [name]: _drop, ...rest } = by;
      return rest;
    });
    return;
  }
  byServer.update((by) => {
    const cur: ServerState = by[name] ?? {
      tasks: [],
      notifications: [],
      error: null,
      taskCursor: null,
      notifCursor: null,
    };
    return { ...by, [name]: { ...cur, ...part } };
  });
}

/** Append the next page of tasks for a resident server, following its cursor. */
export async function loadMoreTasks(name: string): Promise<void> {
  await loadMore(name, "tasks");
}

/** Append the next page of notifications for a resident server. */
export async function loadMoreNotifications(name: string): Promise<void> {
  await loadMore(name, "notifications");
}

const loading = new Set<string>();

async function loadMore(
  name: string,
  which: "tasks" | "notifications",
): Promise<void> {
  const state = get(byServer)[name];
  if (!state) return;
  const cursor = which === "tasks" ? state.taskCursor : state.notifCursor;
  if (cursor === null) return;
  const key = `${name}:${which}`;
  if (loading.has(key)) return;
  loading.add(key);
  try {
    const page =
      which === "tasks"
        ? await listTasks(name, cursor)
        : await listNotifications(name, cursor);
    byServer.update((by) => {
      const cur = by[name];
      if (!cur) return by;
      const merged: ServerState =
        which === "tasks"
          ? {
              ...cur,
              tasks: [...cur.tasks, ...(page.items as Task[])],
              taskCursor: page.next_offset,
            }
          : {
              ...cur,
              notifications: [
                ...cur.notifications,
                ...(page.items as Notification[]),
              ],
              notifCursor: page.next_offset,
            };
      return { ...by, [name]: merged };
    });
    if (which === "notifications") {
      const st = get(byServer)[name];
      if (st) {
        const unread = st.notifications.filter(unreadOf).length;
        summaries.update((m) => ({
          ...m,
          [name]: { error: st.error, unread },
        }));
      }
    }
  } finally {
    loading.delete(key);
  }
}
