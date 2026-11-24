import type { ControlPrefs } from './types';
import type { MessageKey } from './i18n';

/** 手柄按钮：界面顺序、文案键与出厂键位（`KeyboardEvent.code`），与后端 `controlbar::BUTTONS` 一致。 */
export const GAMEPAD_BUTTONS: ReadonlyArray<{
  id: string;
  labelKey: MessageKey;
  defaultKey: string;
}> = [
  { id: 'up', labelKey: 'gamepad.btn.up', defaultKey: 'ArrowUp' },
  { id: 'down', labelKey: 'gamepad.btn.down', defaultKey: 'ArrowDown' },
  { id: 'left', labelKey: 'gamepad.btn.left', defaultKey: 'ArrowLeft' },
  { id: 'right', labelKey: 'gamepad.btn.right', defaultKey: 'ArrowRight' },
  { id: 'a', labelKey: 'gamepad.btn.a', defaultKey: 'KeyJ' },
  { id: 'b', labelKey: 'gamepad.btn.b', defaultKey: 'KeyK' },
  { id: 'x', labelKey: 'gamepad.btn.x', defaultKey: 'KeyU' },
  { id: 'y', labelKey: 'gamepad.btn.y', defaultKey: 'KeyI' },
  { id: 'lb', labelKey: 'gamepad.btn.lb', defaultKey: 'KeyQ' },
  { id: 'rb', labelKey: 'gamepad.btn.rb', defaultKey: 'KeyE' },
  { id: 'lt', labelKey: 'gamepad.btn.lt', defaultKey: 'KeyZ' },
  { id: 'rt', labelKey: 'gamepad.btn.rt', defaultKey: 'KeyC' },
  { id: 'l3', labelKey: 'gamepad.btn.l3', defaultKey: 'KeyR' },
  { id: 'r3', labelKey: 'gamepad.btn.r3', defaultKey: 'KeyT' },
  { id: 'start', labelKey: 'gamepad.btn.start', defaultKey: 'Enter' },
  { id: 'back', labelKey: 'gamepad.btn.back', defaultKey: 'Backspace' },
  { id: 'guide', labelKey: 'gamepad.btn.guide', defaultKey: 'Home' },
];

export const DEFAULT_GAMEPAD_MAP: Record<string, string> = Object.fromEntries(
  GAMEPAD_BUTTONS.map((b) => [b.id, b.defaultKey]),
);

export const DEFAULT_CONTROL: ControlPrefs = {
  barEnabled: true,
  gamepadMap: DEFAULT_GAMEPAD_MAP,
};

const NAMED_KEYS: Record<string, string> = {
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
  Space: 'Space',
  Semicolon: ';',
  Equal: '=',
  Comma: ',',
  Minus: '-',
  Period: '.',
  Slash: '/',
  Backquote: '`',
  BracketLeft: '[',
  BracketRight: ']',
  Backslash: '\\',
  Quote: "'",
  NumpadMultiply: 'Num *',
  NumpadAdd: 'Num +',
  NumpadSubtract: 'Num -',
  NumpadDecimal: 'Num .',
  NumpadDivide: 'Num /',
  NumpadEnter: 'Num Enter',
  ShiftLeft: 'Shift (L)',
  ShiftRight: 'Shift (R)',
  ControlLeft: 'Ctrl (L)',
  ControlRight: 'Ctrl (R)',
  AltLeft: 'Alt (L)',
  AltRight: 'Alt (R)',
};

/** `KeyboardEvent.code` → 界面上给人看的按键名。 */
export function keyLabel(code: string): string {
  if (!code) return '';
  if (NAMED_KEYS[code]) return NAMED_KEYS[code];
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit\d$/.test(code)) return code.slice(5);
  if (/^Numpad\d$/.test(code)) return `Num ${code.slice(6)}`;
  return code;
}
