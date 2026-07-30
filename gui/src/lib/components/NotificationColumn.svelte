<script lang="ts">
  import { goto } from "$app/navigation";
  import { t, locale } from "../i18n";
  import {
    unreadOf,
    settings,
    filterNotifications,
    contentOpenPlan,
    notificationsForView,
    type NotifView,
  } from "../store";
  import { setNotificationRead, markAllRead } from "../api";
  import { refreshNotifications } from "../poller";
  import { onVisible } from "../scroll";
  import { fieldKeys } from "../keys";
  import { openExternal } from "../external";
  import { fmtDateTime, fmtRelative } from "../format";
  import { canToggleRead, supportsTaskDetail } from "../capabilities";
  import type { Notification, ServerInfo } from "../types";

  let {
    notifications = [],
    server,
    hasMore = false,
    onLoadMore = () => {},
  }: {
    notifications?: Notification[];
    server?: ServerInfo;
    hasMore?: boolean;
    onLoadMore?: () => void;
  } = $props();

  // Sorting the resident list: by time (default) or reason, mirroring the task
  // column. Direction defaults to descending (newest / A→Z reversed) and toggles
  // when the active key is clicked again.
  type SortKey = "time" | "reason";
  let sortKey = $state<SortKey>("time");
  let sortDir = $state<"asc" | "desc">("desc");
  const sortOptions: { key: SortKey; label: string }[] = $derived([
    { key: "time", label: $t("sort.updated") },
    { key: "reason", label: $t("sort.reason") },
  ]);
  function setSort(key: SortKey): void {
    if (key === sortKey) sortDir = sortDir === "desc" ? "asc" : "desc";
    else {
      sortKey = key;
      sortDir = "desc";
    }
    limit = PAGE;
  }
  function cmpAsc(a: Notification, b: Notification): number {
    const key = sortKey === "time" ? "updatedAt" : "reason";
    return String(a[key] ?? "").localeCompare(String(b[key] ?? ""));
  }
  function sortNotifs(list: Notification[]): Notification[] {
    const mul = sortDir === "asc" ? 1 : -1;
    return [...list].sort((a, b) => mul * cmpAsc(a, b));
  }

  // Text filter, local to this column (the task column has its own), matched
  // across all fields (subject, reason, project, …).
  let filter = $state("");
  // Read/unread view, to triage handled from pending: "unread" hides notifications
  // already marked read, "all" shows everything. Defaults to unread so the column
  // opens on what still needs attention. Local to this column.
  let view = $state<NotifView>("unread");
  function setView(v: NotifView): void {
    view = v;
    limit = PAGE;
  }
  const shown = $derived(
    sortNotifs(
      notificationsForView(filterNotifications(notifications, filter), view),
    ),
  );

  // Windowed rendering: reveal a page at a time, then fetch the next backend
  // page once the resident list is exhausted.
  const PAGE = 50;
  let limit = $state(PAGE);
  const visible = $derived(shown.slice(0, limit));
  const canReveal = $derived(limit < shown.length);

  function loadMore(): void {
    if (canReveal) limit = Math.min(limit + PAGE, shown.length);
    else if (hasMore) onLoadMore();
  }

  // Read/unread toggling is a backend capability (only some backends expose a
  // per-notification read write).
  const canToggle = $derived(canToggleRead(server));
  // Async feedback (project rule): show which dot is in flight, disable it while
  // the toggle runs. `busyId` is the notification being toggled; `busyAll` marks
  // the mark-all action. Any in-flight action blocks the others, including the
  // reload that follows one — but the reload reports itself through the sync bar
  // rather than a second spinner here, so one phase never lights two indicators.
  let busyId = $state<number | null>(null);
  let busyAll = $state(false);
  let reloading = $state(false);
  const anyBusy = $derived(busyId !== null || busyAll || reloading);

  /** Reload the column after a read-state write. Only notifications can have
   * changed, so the task list is left alone. */
  async function reload(name: string): Promise<void> {
    reloading = true;
    try {
      await refreshNotifications(name);
    } finally {
      reloading = false;
    }
  }

  async function toggle(n: Notification): Promise<void> {
    if (!server || anyBusy) return;
    busyId = Number(n.id);
    try {
      await setNotificationRead(server.name, Number(n.id), unreadOf(n));
    } finally {
      busyId = null;
    }
    await reload(server.name);
  }

  async function markAll(): Promise<void> {
    if (!server || anyBusy) return;
    busyAll = true;
    try {
      await markAllRead(server.name);
    } finally {
      busyAll = false;
    }
    await reload(server.name);
  }

  // Opening the notification's work package on the task-detail screen is a
  // backend capability (same one the task list uses to make subjects clickable).
  const canDetail = $derived(supportsTaskDetail(server));
  // The related work package id a notification points at, or null when absent
  // (the notification is not about a work package, or the backend omits it).
  function wpIdOf(n: Notification): number | null {
    return n.wpId;
  }
  function openTask(n: Notification): void {
    const id = wpIdOf(n);
    if (!server || id == null) return;
    goto(
      `/task?server=${encodeURIComponent(server.name)}&id=${encodeURIComponent(String(id))}`,
    );
  }
  // Browser URL for a notification whose task has no in-app detail screen
  // (GitHub): the subject's resolved web address (`url`), or null when absent.
  function hrefOf(n: Notification): string | null {
    return n.url ?? null;
  }

  // Where a notification's subject opens on click: the server's effective
  // preference, honored the same way as the task list (`contentOpenPlan`). The
  // in-app screen is only reachable when the backend supports task detail and the
  // notification points at a work package.
  const openTarget = $derived(server?.open_content_in ?? "browser");
  function plan(n: Notification) {
    return contentOpenPlan({
      openTarget,
      canDetail: canDetail && wpIdOf(n) != null,
      hasHref: hrefOf(n) != null,
    });
  }
  function openBrowser(n: Notification): void {
    const href = hrefOf(n);
    if (href) openExternal(href);
  }
  function openPrimary(n: Notification): void {
    if (plan(n).primary === "app") openTask(n);
    else openBrowser(n);
  }

  // Semantic tint for a CI (CheckSuite) notification, by the run outcome the
  // backend derived: a failed run reads as a warning, a successful run as good.
  // Empty for non-CI notifications (no `outcome`).
  function ciTone(n: Notification): string {
    if (n.outcome === "failure") return "ci-fail";
    if (n.outcome === "success") return "ci-ok";
    return "";
  }

  // The notification's timestamp as an ISO string, or "" when absent.
  function tsOf(n: Notification): string {
    return n.updatedAt ?? "";
  }
  function tsAbsolute(n: Notification): string {
    const iso = tsOf(n);
    return iso ? fmtDateTime(iso, $locale, $settings.timezone) : "";
  }
  function tsRelative(n: Notification): string {
    const iso = tsOf(n);
    return iso ? fmtRelative(iso, $locale) : "";
  }
  // The primary label follows the `relative_times` setting (absolute by
  // default); the alternate form is offered on hover via the title.
  function tsPrimary(n: Notification): string {
    return $settings.relative_times ? tsRelative(n) : tsAbsolute(n);
  }
  function tsAlternate(n: Notification): string {
    return $settings.relative_times ? tsAbsolute(n) : tsRelative(n);
  }
</script>

<section class="card" aria-label={$t("col.notifications")}>
  <header>
    <h2>{$t("col.notifications")}</h2>
    {#if canToggle}
      <button
        type="button"
        class="btn ghost btn-sm"
        class:busy={busyAll}
        disabled={anyBusy}
        aria-busy={busyAll}
        onclick={markAll}
      >
        {#if busyAll}<span class="spinner" aria-hidden="true"></span>{/if}
        {$t("notif.markAll")}</button
      >
    {/if}
  </header>
  {#if notifications.length === 0}
    <p class="empty">{$t("empty.notifications")}</p>
  {:else}
    <div class="sortbar">
      <span class="sort-label">{$t("sort.by")}</span>
      <span class="seg">
        {#each sortOptions as opt (opt.key)}
          <button
            type="button"
            aria-pressed={sortKey === opt.key}
            title={sortKey === opt.key
              ? $t(sortDir === "desc" ? "sort.dir.desc" : "sort.dir.asc")
              : opt.label}
            onclick={() => setSort(opt.key)}
            >{opt.label}{#if sortKey === opt.key}<span
                class="sort-arrow"
                aria-hidden="true">{sortDir === "desc" ? " ↓" : " ↑"}</span
              >{/if}</button
          >
        {/each}
      </span>
      <span class="seg view-seg">
        <button
          type="button"
          aria-pressed={view === "unread"}
          onclick={() => setView("unread")}>{$t("notif.viewUnread")}</button
        >
        <button
          type="button"
          aria-pressed={view === "all"}
          onclick={() => setView("all")}>{$t("notif.viewAll")}</button
        >
      </span>
    </div>
    <div class="filterbar">
      <input
        type="search"
        aria-label={$t("filter.notifications")}
        placeholder={$t("filter.notifications")}
        bind:value={filter}
        use:fieldKeys={() => ""}
      />
      <span class="filtercount">{shown.length}</span>
    </div>
    {#if shown.length === 0}
      <p class="empty">{$t("empty.notifications")}</p>
    {/if}
    <ul class="list">
      {#each visible as n (n.id)}
        <li class="notif {ciTone(n)}" class:unread={unreadOf(n)}>
          {#if canToggle}
            <button
              type="button"
              class="readdot"
              class:unread={unreadOf(n)}
              class:busy={busyId === Number(n.id)}
              disabled={anyBusy}
              aria-busy={busyId === Number(n.id)}
              aria-label={unreadOf(n)
                ? $t("notif.markRead")
                : $t("notif.markUnread")}
              title={unreadOf(n)
                ? $t("notif.markRead")
                : $t("notif.markUnread")}
              onclick={() => toggle(n)}
            ></button>
          {:else}
            <span
              class="readdot"
              class:unread={unreadOf(n)}
              aria-hidden="true"
              title={unreadOf(n) ? $t("notif.isUnread") : $t("notif.isRead")}
            ></span>
          {/if}
          <span class="reason">{n.reason}</span>
          {#if plan(n).primary !== "none"}
            <button
              type="button"
              class="subject subject-btn"
              onclick={() => openPrimary(n)}
              title={plan(n).primary === "app"
                ? $t("task.openDetail")
                : (hrefOf(n) ?? "")}>{n.title}</button
            >
          {:else}
            <span class="subject">{n.title}</span>
          {/if}
          {#if plan(n).secondaryBrowser}
            <button
              type="button"
              class="openbtn"
              onclick={() => openBrowser(n)}
              title={$t("content.openInBrowser")}
              aria-label={$t("content.openInBrowser")}>↗</button
            >
          {/if}
          {#if tsOf(n)}
            <time class="notif-time" datetime={tsOf(n)} title={tsAlternate(n)}
              >{tsPrimary(n)}</time
            >
          {/if}
        </li>
      {/each}
    </ul>
    {#if canReveal || hasMore}
      <div class="sentinel" use:onVisible={loadMore}></div>
      <button type="button" class="linkbtn more" onclick={loadMore}>
        {$t("list.loadMore")}
      </button>
    {/if}
  {/if}
</section>
