import { describe, it, expect, beforeEach, vi } from "vitest";
import { get } from "svelte/store";
import type { Page, Task, Notification } from "./types";

const emptyPage = <T>(): Page<T> => ({ items: [], next_offset: null });

// Mock the Tauri command layer so the poller exercises pure store logic.
vi.mock("./api", () => ({
  listServers: vi.fn(),
  listTasks: vi.fn(async () => emptyPage<Task>()),
  listNotifications: vi.fn(async () => emptyPage<Notification>()),
  getTimelog: vi.fn(async () => null),
  notifyItems: vi.fn(async () => {}),
}));

import * as api from "./api";
import {
  refreshAll,
  refreshOnResume,
  refreshServer,
  refreshNotifications,
  loadMoreTasks,
} from "./poller";
import { servers, byServer, summaries, activeServer } from "./store";
import { makeTask, makeNotif } from "./test-fixtures";
import type { ServerInfo } from "./types";

function srv(name: string, enabled: boolean): ServerInfo {
  return {
    name,
    base_url: `https://${name}`,
    backend: "openproject",
    enabled,
    is_default: false,
    poll_secs: 60,
  } as ServerInfo;
}

const page = <T>(items: T[], next: number | null): Page<T> => ({
  items,
  next_offset: next,
});

describe("refreshAll", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    servers.set([]);
    byServer.set({});
    summaries.set({});
    activeServer.set(null);
  });

  it("polls only enabled servers and the timelog", async () => {
    servers.set([srv("a", true), srv("b", false), srv("c", true)]);
    await refreshAll();
    const polled = vi
      .mocked(api.listTasks)
      .mock.calls.map((c) => c[0])
      .sort();
    expect(polled).toEqual(["a", "c"]);
    expect(api.getTimelog).toHaveBeenCalledTimes(1);
  });

  it("skips a resume refresh that follows one just finished", async () => {
    servers.set([srv("a", true)]);
    await refreshAll();
    const after = vi.mocked(api.listTasks).mock.calls.length;
    await refreshOnResume();
    // Within the gap the focus-triggered refresh is a no-op...
    expect(vi.mocked(api.listTasks).mock.calls.length).toBe(after);
    // ...and it runs again once enough time has passed.
    vi.setSystemTime(Date.now() + 61_000);
    await refreshOnResume();
    expect(vi.mocked(api.listTasks).mock.calls.length).toBeGreaterThan(after);
  });

  it("shows tasks before the notification request settles", async () => {
    servers.set([srv("a", true)]);
    activeServer.set("a");
    let releaseNotifs: () => void = () => {};
    vi.mocked(api.listNotifications).mockImplementationOnce(
      () =>
        new Promise(
          (r) =>
            (releaseNotifs = () => r(page([makeNotif({ id: "n1" })], null))),
        ),
    );
    vi.mocked(api.listTasks).mockResolvedValueOnce(page([makeTask()], null));
    const polling = refreshAll();
    // Let the resolved task request settle while notifications are still open.
    await Promise.resolve();
    await Promise.resolve();
    expect(get(byServer).a?.tasks).toHaveLength(1);
    expect(get(byServer).a?.notifications ?? []).toHaveLength(0);
    releaseNotifs();
    await polling;
    expect(get(byServer).a?.notifications).toHaveLength(1);
  });

  it("guards against overlapping runs", async () => {
    servers.set([srv("a", true)]);
    let release: () => void = () => {};
    vi.mocked(api.listTasks).mockImplementationOnce(
      () => new Promise((r) => (release = () => r(emptyPage<Task>()))),
    );
    const first = refreshAll();
    const second = refreshAll(); // must return early while `first` is pending
    release();
    await Promise.all([first, second]);
    expect(api.getTimelog).toHaveBeenCalledTimes(1);
  });
});

describe("residency and summaries", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    servers.set([srv("a", true), srv("b", true)]);
    byServer.set({});
    summaries.set({});
    activeServer.set("a");
  });

  it("keeps full arrays only for the active server, summaries for all", async () => {
    vi.mocked(api.listNotifications).mockResolvedValue(
      page<Notification>(
        [makeNotif({ read: false }), makeNotif({ reason: "x" })],
        null,
      ),
    );
    vi.mocked(api.listTasks).mockResolvedValue(
      page<Task>([makeTask({ id: { display: "#1", raw: "1" } })], null),
    );
    await refreshServer("a");
    await refreshServer("b");

    // Active server 'a' resident; 'b' evicted to summary only.
    expect(get(byServer).a?.tasks.length).toBe(1);
    expect(get(byServer).b).toBeUndefined();
    // Both servers carry an unread summary (both notifications count as unread).
    expect(get(summaries).a?.unread).toBe(2);
    expect(get(summaries).b?.unread).toBe(2);
  });

  it("refreshNotifications reloads only the notification column", async () => {
    vi.mocked(api.listTasks).mockResolvedValue(
      page<Task>([makeTask({ id: { display: "#1", raw: "1" } })], null),
    );
    vi.mocked(api.listNotifications).mockResolvedValue(
      page<Notification>([makeNotif({ read: false })], null),
    );
    await refreshServer("a");
    vi.clearAllMocks();

    vi.mocked(api.listNotifications).mockResolvedValue(
      page<Notification>(
        [
          makeNotif({ id: "1", read: true }),
          makeNotif({ id: "2", read: true }),
        ],
        null,
      ),
    );
    await refreshNotifications("a");
    // The task request is not repeated; a read-state write cannot change tasks.
    expect(api.listTasks).not.toHaveBeenCalled();
    expect(get(byServer).a?.notifications).toHaveLength(2);
    expect(get(byServer).a?.tasks).toHaveLength(1);
    expect(get(summaries).a?.unread).toBe(0);
  });

  it("records the error in the summary on failure", async () => {
    vi.mocked(api.listNotifications).mockRejectedValueOnce(new Error("boom"));
    await refreshServer("a");
    expect(get(summaries).a?.error).toContain("boom");
  });
});

describe("loadMoreTasks", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    servers.set([srv("a", true)]);
    summaries.set({});
    activeServer.set("a");
    byServer.set({
      a: {
        tasks: [makeTask({ id: { display: "#1", raw: "1" } })],
        notifications: [],
        error: null,
        taskCursor: 2,
        notifCursor: null,
      },
    });
  });

  it("appends the next page and advances the cursor", async () => {
    vi.mocked(api.listTasks).mockResolvedValueOnce(
      page<Task>([makeTask({ id: { display: "#2", raw: "2" } })], 3),
    );
    await loadMoreTasks("a");
    expect(vi.mocked(api.listTasks)).toHaveBeenCalledWith("a", 2);
    expect(get(byServer).a.tasks.map((t) => t.id.display)).toEqual([
      "#1",
      "#2",
    ]);
    expect(get(byServer).a.taskCursor).toBe(3);
  });

  it("does nothing when the cursor is null (list complete)", async () => {
    byServer.update((by) => ({ ...by, a: { ...by.a, taskCursor: null } }));
    await loadMoreTasks("a");
    expect(vi.mocked(api.listTasks)).not.toHaveBeenCalled();
  });
});
