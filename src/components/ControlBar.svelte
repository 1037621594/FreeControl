<script lang="ts">
  // 投屏控制条：贴在 scrcpy 窗口下方的独立小窗口（由后端按会话创建与定位）。
  // 三个导航键直接经 adb 发到设备；手柄键开关切换该设备的键盘 → 手柄映射。
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import {
    barContext,
    getGamepadModes,
    setGamepadMode,
    sendNavKey,
    openGamepadSettings,
    EVENT_GAMEPAD_CHANGED,
  } from '../lib/api';
  import { t } from '../lib/i18n';
  import type { BarContext, GamepadChanged } from '../lib/types';
  import { ChevronLeft, Circle, Square, Gamepad2, Settings2 } from 'lucide-svelte';

  let ctx: BarContext | null = null;
  let on = false;

  async function nav(key: 'back' | 'home' | 'recents'): Promise<void> {
    if (!ctx) return;
    try {
      await sendNavKey(ctx.deviceId, key);
    } catch (e) {
      console.error('发送导航键失败', e);
    }
  }

  async function toggleGamepad(): Promise<void> {
    if (!ctx) return;
    const next = !on;
    try {
      await setGamepadMode(ctx.deviceId, next);
      on = next;
    } catch (e) {
      console.error('切换手柄模式失败', e);
    }
  }

  async function openSettings(): Promise<void> {
    try {
      await openGamepadSettings();
    } catch (e) {
      console.error('打开手柄设置失败', e);
    }
  }

  onMount(() => {
    let unlisten: (() => void) | null = null;
    let destroyed = false;
    void (async () => {
      try {
        ctx = await barContext();
        on = (await getGamepadModes()).includes(ctx.deviceId);
        const off = await listen<GamepadChanged>(EVENT_GAMEPAD_CHANGED, (event) => {
          if (event.payload.deviceId === ctx?.deviceId) on = event.payload.on;
        });
        if (destroyed) off();
        else unlisten = off;
      } catch (e) {
        console.error('初始化控制条失败', e);
      }
    })();
    return () => {
      destroyed = true;
      unlisten?.();
    };
  });
</script>

<div class="cb-bar" role="toolbar" aria-label={$t('bar.label')}>
  <button class="cb-btn" type="button" title={$t('bar.back')} aria-label={$t('bar.back')} on:click={() => nav('back')}>
    <ChevronLeft />
  </button>
  <button class="cb-btn" type="button" title={$t('bar.home')} aria-label={$t('bar.home')} on:click={() => nav('home')}>
    <Circle />
  </button>
  <button class="cb-btn" type="button" title={$t('bar.recents')} aria-label={$t('bar.recents')} on:click={() => nav('recents')}>
    <Square />
  </button>
  <span class="cb-sep" aria-hidden="true"></span>
  <button
    class="cb-btn"
    class:active={on}
    type="button"
    aria-pressed={on}
    title={on ? $t('bar.gamepadOn') : $t('bar.gamepadOff')}
    aria-label={on ? $t('bar.gamepadOn') : $t('bar.gamepadOff')}
    on:click={toggleGamepad}
  >
    <Gamepad2 />
  </button>
  <button class="cb-btn" type="button" title={$t('bar.settings')} aria-label={$t('bar.settings')} on:click={openSettings}>
    <Settings2 />
  </button>
</div>
