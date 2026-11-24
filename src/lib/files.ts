import { writable } from 'svelte/store';

/** 设备文件剪贴板：只在同一台设备内粘贴，换设备后粘贴按钮禁用。 */
export interface FileClipboard {
  deviceId: string;
  mode: 'copy' | 'move';
  paths: string[];
}

export const fileClipboard = writable<FileClipboard | null>(null);

/** 弹窗首次打开时进入的目录。 */
export const FILES_HOME = '/sdcard';

/** 每台设备上次浏览到的目录（本次运行内），再次打开时回到那里。 */
export const lastFilePath: Record<string, string> = {};

/** 本地路径（Windows 反斜杠或 POSIX 斜杠）的末段。 */
export function baseName(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

export function joinDevicePath(dir: string, name: string): string {
  return dir === '/' ? `/${name}` : `${dir}/${name}`;
}

export function parentDevicePath(path: string): string {
  const i = path.lastIndexOf('/');
  return i <= 0 ? '/' : path.slice(0, i);
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[i]}`;
}
