<script lang="ts">
  // 设备文件浏览弹窗：外壳与应用列表弹窗同款。目录列表、新建 / 重命名 / 删除、设备内复制 / 剪切 / 粘贴，
  // 以及与电脑互传（按钮选择或把文件拖进窗口）。同名项目默认不覆盖，由用户确认后才覆盖。
  import { createEventDispatcher, onMount } from 'svelte';
  import { fade } from 'svelte/transition';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import { open } from '@tauri-apps/plugin-dialog';
  import { DIALOG_FADE, DIALOG_SCALE, dialogScale } from '../lib/motion';
  import { backdropClose } from '../lib/backdrop';
  import { devices } from '../lib/store';
  import {
    listDeviceFiles,
    makeDeviceDir,
    renameDeviceFile,
    deleteDeviceFiles,
    pasteDeviceFiles,
    pullDeviceFiles,
    pushDeviceFiles,
    cancelFileTransfer,
  } from '../lib/api';
  import {
    FILES_HOME,
    baseName,
    fileClipboard,
    formatSize,
    joinDevicePath,
    lastFilePath,
    parentDevicePath,
  } from '../lib/files';
  import { t, resolvedLang, langTag } from '../lib/i18n';
  import { errorTextOf } from '../lib/message';
  import { tip } from '../lib/tooltip';
  import type { FileEntry, FileOpFailure, FileOpOutcome } from '../lib/types';
  import {
    X,
    Maximize2,
    Minimize2,
    RefreshCw,
    ArrowUp,
    Folder,
    FolderOpen,
    FolderPlus,
    FolderUp,
    File as FileIcon,
    FileUp,
    Download,
    Copy,
    Scissors,
    ClipboardPaste,
    Pencil,
    Trash2,
    AlertTriangle,
    Link2,
  } from 'lucide-svelte';

  export let deviceId: string;

  const dispatch = createEventDispatcher<{ close: void }>();

  type Task = (items: string[], overwrite: boolean) => Promise<FileOpOutcome>;
  type Acc = { ok: number; failed: number };
  type PendingConflict = {
    kind: 'work' | 'transfer';
    task: Task;
    items: string[];
    acc: Acc;
    after?: () => void;
  };

  let expanded = false;
  let path = lastFilePath[deviceId] ?? FILES_HOME;
  let pathDraft = path;
  let entries: FileEntry[] = [];
  let loading = false;
  let loaded = false;
  let loadError: unknown = null;
  let selected = new Set<string>();
  let busy: 'work' | 'transfer' | null = null;
  let feedback = '';
  let failures: FileOpFailure[] = [];
  let actionError: unknown = null;
  let edit: { kind: 'mkdir' | 'rename'; value: string; target: string } | null = null;
  let confirmDelete = false;
  let conflict: PendingConflict | null = null;
  let dragging = false;
  let loadSeq = 0;

  $: alias = $devices.find((d) => d.id === deviceId)?.alias ?? '';
  $: locale = langTag($resolvedLang);
  $: sorted = [...entries].sort((a, b) =>
    a.isDir === b.isDir ? a.name.localeCompare(b.name, locale) : a.isDir ? -1 : 1,
  );
  $: hasSel = selected.size > 0;
  $: allSelected = sorted.length > 0 && selected.size === sorted.length;
  $: canPaste = !!$fileClipboard && $fileClipboard.deviceId === deviceId;
  $: conflictNames =
    conflict === null
      ? ''
      : conflict.items.slice(0, 5).map(baseName).join(', ') + (conflict.items.length > 5 ? '…' : '');

  async function load(target: string): Promise<void> {
    const seq = ++loadSeq;
    loading = true;
    loadError = null;
    try {
      const res = await listDeviceFiles(deviceId, target);
      if (seq !== loadSeq) return;
      const names = new Set(res.entries.map((e) => e.name));
      selected =
        res.path === path
          ? new Set([...selected].filter((n) => names.has(n)))
          : new Set<string>();
      path = res.path;
      pathDraft = res.path;
      entries = res.entries;
      loaded = true;
      lastFilePath[deviceId] = res.path;
    } catch (e) {
      if (seq !== loadSeq) return;
      loadError = e;
      pathDraft = path;
    } finally {
      if (seq === loadSeq) loading = false;
    }
  }

  function resetMessages(): void {
    feedback = '';
    failures = [];
    actionError = null;
    edit = null;
    confirmDelete = false;
    conflict = null;
  }

  function go(target: string): void {
    if (busy) return;
    resetMessages();
    void load(target);
  }

  function onPathKeydown(e: KeyboardEvent): void {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    go(pathDraft.trim() || FILES_HOME);
  }

  function toggle(name: string): void {
    const next = new Set(selected);
    if (next.has(name)) next.delete(name);
    else next.add(name);
    selected = next;
  }

  function toggleAll(): void {
    selected = allSelected ? new Set<string>() : new Set(sorted.map((e) => e.name));
  }

  function onRowClick(e: MouseEvent, name: string): void {
    if (e.ctrlKey || e.metaKey) toggle(name);
    else selected = new Set([name]);
  }

  function openEntry(entry: FileEntry): void {
    if (entry.isDir && !busy) go(joinDevicePath(path, entry.name));
  }

  function onRowKeydown(e: KeyboardEvent, entry: FileEntry): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      openEntry(entry);
    } else if (e.key === ' ') {
      e.preventDefault();
      toggle(entry.name);
    }
  }

  function selectedPaths(): string[] {
    return sorted.filter((e) => selected.has(e.name)).map((e) => joinDevicePath(path, e.name));
  }

  /** 统一执行一次批量操作：累计结果、遇到同名项目时挂起等用户确认，结束后刷新当前目录。 */
  async function run(
    kind: 'work' | 'transfer',
    task: Task,
    items: string[],
    overwrite = false,
    acc: Acc = { ok: 0, failed: 0 },
    after?: () => void,
  ): Promise<void> {
    busy = kind;
    actionError = null;
    conflict = null;
    if (!overwrite) {
      feedback = '';
      failures = [];
    }
    let outcome: FileOpOutcome | null = null;
    try {
      outcome = await task(items, overwrite);
    } catch (e) {
      actionError = e;
    } finally {
      busy = null;
    }
    if (outcome) {
      acc.ok += outcome.ok;
      acc.failed += outcome.failed.length;
      failures = [...failures, ...outcome.failed];
      if (outcome.canceled) {
        feedback = $t('files.canceled', { n: acc.ok });
      } else if (outcome.conflicts.length) {
        conflict = { kind, task, items: outcome.conflicts, acc, after };
      } else {
        summarize(acc);
        if (acc.failed === 0) after?.();
      }
    }
    await load(path);
  }

  function summarize(acc: Acc): void {
    feedback =
      acc.failed > 0
        ? $t('files.doneFailed', { n: acc.ok, m: acc.failed })
        : $t('files.done', { n: acc.ok });
  }

  function resolveConflict(overwrite: boolean): void {
    const c = conflict;
    if (!c) return;
    if (overwrite) {
      void run(c.kind, c.task, c.items, true, c.acc, c.after);
      return;
    }
    conflict = null;
    summarize(c.acc);
    if (c.acc.failed === 0) c.after?.();
  }

  function copySelected(mode: 'copy' | 'move'): void {
    const paths = selectedPaths();
    if (!paths.length) return;
    fileClipboard.set({ deviceId, mode, paths });
    resetMessages();
    feedback = $t(mode === 'copy' ? 'files.clipboard.copy' : 'files.clipboard.move', {
      n: paths.length,
    });
  }

  function paste(): void {
    const clip = $fileClipboard;
    if (!clip || clip.deviceId !== deviceId || busy) return;
    const dest = path;
    void run(
      'work',
      (items, overwrite) =>
        pasteDeviceFiles({ deviceId, sources: items, destDir: dest, mode: clip.mode, overwrite }),
      clip.paths,
      false,
      undefined,
      clip.mode === 'move' ? () => fileClipboard.set(null) : undefined,
    );
  }

  function startMkdir(): void {
    if (busy) return;
    resetMessages();
    edit = { kind: 'mkdir', value: '', target: '' };
  }

  function startRename(): void {
    if (busy || selected.size !== 1) return;
    const name = [...selected][0];
    resetMessages();
    edit = { kind: 'rename', value: name, target: joinDevicePath(path, name) };
  }

  async function submitEdit(): Promise<void> {
    if (!edit || busy) return;
    const current = edit;
    const name = current.value.trim();
    if (!name) return;
    busy = 'work';
    actionError = null;
    let ok = false;
    try {
      if (current.kind === 'mkdir') await makeDeviceDir(deviceId, path, name);
      else await renameDeviceFile(deviceId, current.target, name);
      ok = true;
    } catch (e) {
      actionError = e;
    } finally {
      busy = null;
    }
    if (ok) {
      edit = null;
      if (current.kind === 'rename') selected = new Set([name]);
      await load(path);
    }
  }

  function startDelete(): void {
    if (busy || !hasSel) return;
    resetMessages();
    confirmDelete = true;
  }

  function doDelete(): void {
    const items = selectedPaths();
    confirmDelete = false;
    void run('work', (list) => deleteDeviceFiles(deviceId, list), items);
  }

  async function download(): Promise<void> {
    const items = selectedPaths();
    if (!items.length || busy) return;
    let picked: string | string[] | null = null;
    try {
      picked = await open({ directory: true, multiple: false, title: $t('files.chooseSaveDir') });
    } catch (e) {
      actionError = e;
      return;
    }
    if (typeof picked !== 'string' || !picked) return;
    const localDir = picked;
    void run(
      'transfer',
      (list, overwrite) => pullDeviceFiles({ deviceId, paths: list, localDir, overwrite }),
      items,
    );
  }

  function uploadPaths(paths: string[]): void {
    if (!paths.length || busy) return;
    resetMessages();
    const dest = path;
    void run(
      'transfer',
      (list, overwrite) => pushDeviceFiles({ deviceId, localPaths: list, destDir: dest, overwrite }),
      paths,
    );
  }

  async function pickUpload(directory: boolean): Promise<void> {
    if (busy) return;
    let picked: string | string[] | null = null;
    try {
      picked = await open({
        directory,
        multiple: !directory,
        title: $t(directory ? 'files.chooseUploadDir' : 'files.chooseUploadFiles'),
      });
    } catch (e) {
      actionError = e;
      return;
    }
    if (!picked) return;
    uploadPaths(Array.isArray(picked) ? picked : [picked]);
  }

  function cancelTransfer(): void {
    void cancelFileTransfer(deviceId).catch(() => {});
  }

  function close(): void {
    dispatch('close');
  }

  function onBackdrop(): void {
    if (!busy) close();
  }

  function onKeydown(e: KeyboardEvent): void {
    if (e.key !== 'Escape') return;
    if (edit) {
      edit = null;
    } else if (confirmDelete) {
      confirmDelete = false;
    } else if (conflict) {
      resolveConflict(false);
    } else if (!busy) {
      close();
    }
  }

  function focusSelect(node: HTMLInputElement): void {
    node.focus();
    node.select();
  }

  onMount(() => {
    void load(path);
    let destroyed = false;
    let unlisten: (() => void) | null = null;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        const p = event.payload;
        if (p.type === 'enter' || p.type === 'over') {
          dragging = !busy && !edit;
        } else if (p.type === 'leave') {
          dragging = false;
        } else if (p.type === 'drop') {
          dragging = false;
          if (!busy && !edit && p.paths.length) uploadPaths(p.paths);
        }
      })
      .then((fn) => {
        if (destroyed) fn();
        else unlisten = fn;
      })
      .catch(() => {});
    return () => {
      destroyed = true;
      unlisten?.();
    };
  });
</script>

<svelte:window on:keydown={onKeydown} />

<div class="overlay dp-overlay app-list-overlay show" transition:fade={DIALOG_FADE}>
  <div class="overlay-backdrop" role="presentation" use:backdropClose={onBackdrop} />
  <div
    class="modal dp-modal files-modal"
    class:expanded
    transition:dialogScale={DIALOG_SCALE}
    role="dialog"
    aria-modal="true"
    aria-label={$t('files.title')}
  >
    <header class="dp-head" data-tauri-drag-region="deep">
      <span class="dp-title">
        <Folder />
        <span class="dp-title-text">{$t('files.title')}{alias ? ` · ${alias}` : ''}</span>
      </span>
      <span class="dp-actions">
        <span class="tip-host" use:tip={expanded ? $t('device.params.collapse') : $t('device.params.expand')}>
          <button
            class="dp-icon-btn"
            type="button"
            aria-label={expanded ? $t('device.params.collapse') : $t('device.params.expand')}
            aria-pressed={expanded}
            on:click={() => (expanded = !expanded)}
          >
            {#if expanded}<Minimize2 />{:else}<Maximize2 />{/if}
          </button>
        </span>
        <button
          class="dp-icon-btn"
          type="button"
          aria-label={$t('about.close')}
          disabled={!!busy}
          on:click={close}
        >
          <X />
        </button>
      </span>
    </header>

    <div class="dp-body al-body fl-body">
      <div class="al-toolbar">
        <span class="tip-host" use:tip={$t('files.up')}>
          <button
            class="btn ghost sm fl-icon"
            type="button"
            aria-label={$t('files.up')}
            disabled={!!busy || path === '/'}
            on:click={() => go(parentDevicePath(path))}
          >
            <ArrowUp />
          </button>
        </span>
        <label class="al-search">
          <FolderOpen />
          <input
            type="text"
            bind:value={pathDraft}
            aria-label={$t('files.pathLabel')}
            spellcheck="false"
            autocomplete="off"
            disabled={!!busy}
            on:keydown={onPathKeydown}
          />
        </label>
        <button
          class="btn ghost sm al-refresh"
          class:busy={loading}
          type="button"
          disabled={loading || !!busy}
          on:click={() => go(path)}
        >
          <RefreshCw />
          {$t('files.refresh')}
        </button>
      </div>

      <div class="fl-actions">
        <button class="btn ghost sm" type="button" disabled={!!busy} on:click={startMkdir}>
          <FolderPlus />
          {$t('files.newFolder')}
        </button>
        <button class="btn ghost sm" type="button" disabled={!!busy} on:click={() => pickUpload(false)}>
          <FileUp />
          {$t('files.uploadFiles')}
        </button>
        <button class="btn ghost sm" type="button" disabled={!!busy} on:click={() => pickUpload(true)}>
          <FolderUp />
          {$t('files.uploadFolder')}
        </button>
        <button class="btn ghost sm" type="button" disabled={!!busy || !hasSel} on:click={download}>
          <Download />
          {$t('files.download')}
        </button>
        <span class="fl-sep" aria-hidden="true"></span>
        <button class="btn ghost sm" type="button" disabled={!!busy || !hasSel} on:click={() => copySelected('copy')}>
          <Copy />
          {$t('files.copy')}
        </button>
        <button class="btn ghost sm" type="button" disabled={!!busy || !hasSel} on:click={() => copySelected('move')}>
          <Scissors />
          {$t('files.cut')}
        </button>
        <button class="btn ghost sm" type="button" disabled={!!busy || !canPaste} on:click={paste}>
          <ClipboardPaste />
          {$t('files.paste')}
        </button>
        <span class="fl-sep" aria-hidden="true"></span>
        <button class="btn ghost sm" type="button" disabled={!!busy || selected.size !== 1} on:click={startRename}>
          <Pencil />
          {$t('files.rename')}
        </button>
        <button class="btn ghost sm fl-danger" type="button" disabled={!!busy || !hasSel} on:click={startDelete}>
          <Trash2 />
          {$t('files.delete')}
        </button>
      </div>

      {#if edit}
        <form class="fl-bar" on:submit|preventDefault={submitEdit}>
          <input
            class="fl-input"
            type="text"
            bind:value={edit.value}
            placeholder={$t('files.newFolderPlaceholder')}
            aria-label={edit.kind === 'mkdir' ? $t('files.newFolder') : $t('files.rename')}
            spellcheck="false"
            autocomplete="off"
            disabled={!!busy}
            use:focusSelect
          />
          <button class="btn primary sm" type="submit" disabled={!!busy || !edit.value.trim()}>
            {$t('files.confirm')}
          </button>
          <button class="btn ghost sm" type="button" disabled={!!busy} on:click={() => (edit = null)}>
            {$t('files.cancel')}
          </button>
        </form>
      {/if}

      {#if confirmDelete}
        <div class="fl-bar fl-bar-warn">
          <span class="fl-bar-text">{$t('files.deleteConfirm', { n: selected.size })}</span>
          <button class="btn primary sm" type="button" on:click={doDelete}>{$t('files.deleteYes')}</button>
          <button class="btn ghost sm" type="button" on:click={() => (confirmDelete = false)}>
            {$t('files.cancel')}
          </button>
        </div>
      {/if}

      {#if conflict}
        <div class="fl-bar fl-bar-warn">
          <span class="fl-bar-text">
            {$t('files.conflict', { n: conflict.items.length, names: conflictNames })}
          </span>
          <button class="btn primary sm" type="button" on:click={() => resolveConflict(true)}>
            {$t('files.overwrite')}
          </button>
          <button class="btn ghost sm" type="button" on:click={() => resolveConflict(false)}>
            {$t('files.skip')}
          </button>
        </div>
      {/if}

      {#if busy}
        <div class="fl-bar">
          <span class="status-spinner" aria-hidden="true"></span>
          <span class="fl-bar-text">
            {busy === 'transfer' ? $t('files.transferring') : $t('files.working')}
          </span>
          {#if busy === 'transfer'}
            <button class="btn ghost sm" type="button" on:click={cancelTransfer}>
              {$t('files.cancelTransfer')}
            </button>
          {/if}
        </div>
      {/if}

      {#if feedback}
        <div class="al-feedback">{feedback}</div>
      {/if}
      {#if actionError}
        <div class="al-error">
          <AlertTriangle />
          <span>{errorTextOf(actionError, $t)}</span>
        </div>
      {/if}
      {#each failures.slice(0, 5) as f}
        <div class="al-error">
          <AlertTriangle />
          <span>{f.name}{$t('common.detailSeparator')}{errorTextOf(f.error, $t)}</span>
        </div>
      {/each}
      {#if failures.length > 5}
        <div class="al-error"><span>…</span></div>
      {/if}
      {#if loadError && loaded}
        <div class="al-error">
          <AlertTriangle />
          <span>{errorTextOf(loadError, $t)}</span>
        </div>
      {/if}

      <div class="fl-wrap">
        <div class="fl-head">
          <input
            type="checkbox"
            aria-label={$t('files.selectAll')}
            checked={allSelected}
            disabled={sorted.length === 0 || !!busy}
            on:change={toggleAll}
          />
          <span>{$t('files.col.name')}</span>
          <span class="fl-num">{$t('files.col.size')}</span>
          <span>{$t('files.col.modified')}</span>
        </div>

        <div class="al-list fl-list">
          {#if loading && !loaded}
            <div class="al-state">
              <span class="status-spinner" aria-hidden="true"></span>
              <span>{$t('files.loading')}</span>
            </div>
          {:else if loadError && !loaded}
            <div class="al-state">
              <span>{errorTextOf(loadError, $t)}</span>
              <button class="btn ghost sm" type="button" on:click={() => go(path)}>
                {$t('files.retry')}
              </button>
            </div>
          {:else if sorted.length === 0}
            <div class="al-state">{$t('files.empty')}</div>
          {:else}
            {#each sorted as entry (entry.name)}
              <div
                class="fl-row"
                class:selected={selected.has(entry.name)}
                role="button"
                tabindex="0"
                on:click={(e) => onRowClick(e, entry.name)}
                on:dblclick={() => openEntry(entry)}
                on:keydown={(e) => onRowKeydown(e, entry)}
              >
                <input
                  type="checkbox"
                  tabindex="-1"
                  aria-label={entry.name}
                  checked={selected.has(entry.name)}
                  disabled={!!busy}
                  on:click|stopPropagation
                  on:change={() => toggle(entry.name)}
                />
                <span class="fl-name" title={entry.name}>
                  {#if entry.isDir}<Folder class="fl-ico dir" />{:else}<FileIcon class="fl-ico" />{/if}
                  <span class="fl-name-text">{entry.name}</span>
                  {#if entry.isLink}<Link2 class="fl-link" />{/if}
                </span>
                <span class="fl-num">{entry.isDir ? '' : formatSize(entry.size)}</span>
                <span class="fl-time">{entry.modified}</span>
              </div>
            {/each}
          {/if}
        </div>

        {#if dragging}
          <div class="fl-drop"><span>{$t('files.dropHint')}</span></div>
        {/if}
      </div>

      <div class="fl-foot">
        <span>{hasSel ? $t('files.selected', { n: selected.size }) : ''}</span>
        <span class="fl-foot-tip">{$t('files.dropTip')}</span>
      </div>
    </div>
  </div>
</div>
