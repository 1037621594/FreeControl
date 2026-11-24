<script lang="ts">
  // 设置页「手柄与控制条」卡：控制条总开关 + 手柄键位。
  // 点按键框后按下想用的键即录入（Esc 取消）；与别的按钮重复时拒绝并提示，不静默覆盖。
  import { onDestroy } from 'svelte';
  import { config, saveGamepadMap, setControlBarEnabled } from '../lib/store';
  import { DEFAULT_GAMEPAD_MAP, GAMEPAD_BUTTONS, keyLabel } from '../lib/gamepad';
  import { t } from '../lib/i18n';
  import { errorTextOf } from '../lib/message';
  import HelpTip from './HelpTip.svelte';
  import { X } from 'lucide-svelte';

  let recording: string | null = null;
  let message = '';
  let busy = false;

  $: barEnabled = $config?.control?.barEnabled ?? true;
  $: map = { ...DEFAULT_GAMEPAD_MAP, ...($config?.control?.gamepadMap ?? {}) } as Record<string, string>;

  function stopRecording(): void {
    recording = null;
    window.removeEventListener('keydown', onKeydown, true);
  }

  function onKeydown(e: KeyboardEvent): void {
    if (!recording) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.code === 'Escape') {
      stopRecording();
      return;
    }
    if (e.code === 'Unidentified' || e.code === '') return;
    const id = recording;
    stopRecording();
    void assign(id, e.code);
  }

  function startRecording(id: string): void {
    if (busy) return;
    message = '';
    stopRecording();
    recording = id;
    window.addEventListener('keydown', onKeydown, true);
  }

  async function assign(id: string, code: string): Promise<void> {
    const owner = GAMEPAD_BUTTONS.find((b) => b.id !== id && map[b.id] === code);
    if (owner) {
      message = $t('gamepad.duplicate', { key: keyLabel(code), button: $t(owner.labelKey) });
      return;
    }
    await commit({ ...map, [id]: code });
  }

  async function clear(id: string): Promise<void> {
    await commit({ ...map, [id]: '' });
  }

  async function resetAll(): Promise<void> {
    await commit({ ...DEFAULT_GAMEPAD_MAP });
  }

  async function commit(next: Record<string, string>): Promise<void> {
    busy = true;
    message = '';
    try {
      await saveGamepadMap(next);
    } catch (e) {
      message = errorTextOf(e, $t);
    } finally {
      busy = false;
    }
  }

  async function onBarToggle(e: Event): Promise<void> {
    const checked = (e.currentTarget as HTMLInputElement).checked;
    try {
      await setControlBarEnabled(checked);
    } catch (err) {
      message = errorTextOf(err, $t);
    }
  }

  onDestroy(stopRecording);
</script>

<div class="card col2">
  <h3>{$t('gamepad.title')}</h3>
  <div class="field">
    <div class="toggle">
      <span class="t-label"
        >{$t('gamepad.bar.label')}<HelpTip title={$t('gamepad.bar.helpTitle')} label={$t('gamepad.bar.helpLabel')}>
          <span class="fhelp-p">{$t('gamepad.bar.help')}</span>
        </HelpTip></span
      >
      <label class="switch">
        <input type="checkbox" checked={barEnabled} on:change={onBarToggle} />
        <span class="slider"></span>
      </label>
    </div>
  </div>
  <div class="field">
    <span class="flabel"
      >{$t('gamepad.map.label')}<HelpTip title={$t('gamepad.map.helpTitle')} label={$t('gamepad.map.helpLabel')}>
        <span class="fhelp-p">{$t('gamepad.map.help')}</span>
      </HelpTip></span
    >
    <div class="gp-grid">
      {#each GAMEPAD_BUTTONS as b (b.id)}
        <div class="gp-item">
          <span class="gp-name">{$t(b.labelKey)}</span>
          <button
            class="gp-key"
            class:recording={recording === b.id}
            class:empty={!map[b.id]}
            type="button"
            disabled={busy}
            on:click={() => startRecording(b.id)}
          >
            {recording === b.id ? $t('gamepad.pressKey') : map[b.id] ? keyLabel(map[b.id]) : $t('gamepad.unbound')}
          </button>
          <button
            class="gp-clear"
            type="button"
            aria-label={$t('gamepad.clear')}
            disabled={busy || !map[b.id]}
            on:click={() => clear(b.id)}
          >
            <X />
          </button>
        </div>
      {/each}
    </div>
  </div>
  <div class="field">
    <div class="path-row">
      <button class="btn sm" type="button" disabled={busy} on:click={resetAll}>
        {$t('gamepad.reset')}
      </button>
      {#if message}
        <span class="hint gp-msg">{message}</span>
      {/if}
    </div>
  </div>
</div>
