const SIZE_UNITS = ['KB', 'MB', 'GB', 'TB']

/**
 * 磁盘占用的人类可读格式，与 Finder 一致的 1000 进制。
 * < 1000 B 显示整数 B；其余保留 1 位小数（四舍五入后若达到 1000，则进位到下一个单位，
 * 避免出现 “1000.0 KB” 这种写法，例如 999999 → 1.0 MB、1000000 → 1.0 MB）。
 */
export function humanSize(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`
  let value = bytes
  let unit = -1
  do {
    value /= 1000
    unit += 1
  } while (Math.round(value * 10) / 10 >= 1000 && unit < SIZE_UNITS.length - 1)
  return `${value.toFixed(1)} ${SIZE_UNITS[unit]}`
}
