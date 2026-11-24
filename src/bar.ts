import './app.css';
import { initLanguage, applyLanguage, DEFAULT_LANGUAGE } from './lib/i18n';
import { initFontStacks } from './lib/fonts';
import { applyAppearance } from './lib/theme';
import { getConfig } from './lib/api';
import ControlBar from './components/ControlBar.svelte';

// 控制条是独立的小窗口：只取配置里的外观与语言，不加载主窗口的 store。
initLanguage();
initFontStacks();

const app = new ControlBar({ target: document.getElementById('bar')! });

void getConfig()
  .then((cfg) => {
    applyAppearance(cfg.ui);
    applyLanguage(cfg.ui.language ?? DEFAULT_LANGUAGE);
  })
  .catch((e) => console.error('读取配置失败', e));

export default app;
