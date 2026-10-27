/** 主题三档：跟随系统 / 始终浅色 / 始终深色。持久化在本机（localStorage）。 */
export type ThemeMode = 'auto' | 'light' | 'dark'

const KEY = 'project-ctrl.theme'

export function loadTheme(): ThemeMode {
  const v = localStorage.getItem(KEY)
  return v === 'light' || v === 'dark' ? v : 'auto'
}

export function saveTheme(mode: ThemeMode) {
  localStorage.setItem(KEY, mode)
}

const media = () => window.matchMedia('(prefers-color-scheme: dark)')

/** 把 mode 解析成实际生效的明暗，并写到 <html class="dark">。 */
export function applyTheme(mode: ThemeMode) {
  const dark = mode === 'dark' || (mode === 'auto' && media().matches)
  document.documentElement.classList.toggle('dark', dark)
  document.documentElement.style.colorScheme = dark ? 'dark' : 'light'
}

export function watchSystemTheme(onChange: () => void): () => void {
  const m = media()
  m.addEventListener('change', onChange)
  return () => m.removeEventListener('change', onChange)
}
