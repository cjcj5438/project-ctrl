import { useEffect, useState } from 'react'
import { relaunch } from '@tauri-apps/plugin-process'
import { check, type Update } from '@tauri-apps/plugin-updater'
import { errorText } from './api'

/**
 * 启动后静默检查一次更新：有新版本才显示横幅，检查失败（断网/404/未发布等）一律忽略。
 * 只有用户点「立即更新」才会下载安装，失败信息显示在横幅内。「稍后」仅关闭横幅。
 */
export default function UpdateBanner() {
  const [update, setUpdate] = useState<Update | null>(null)
  const [installing, setInstalling] = useState(false)
  const [percent, setPercent] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    check()
      .then((u) => {
        if (alive && u) setUpdate(u)
      })
      .catch(() => {
        /* 静默忽略：断网 / 404 / 限流 / 尚未发布 */
      })
    return () => {
      alive = false
    }
  }, [])

  if (!update) return null

  const install = async () => {
    setInstalling(true)
    setError(null)
    let total = 0
    let received = 0
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === 'Started') {
          total = event.data.contentLength ?? 0
          setPercent(total > 0 ? 0 : null)
        } else if (event.event === 'Progress') {
          received += event.data.chunkLength
          if (total > 0) setPercent(Math.min(100, Math.round((received / total) * 100)))
        } else {
          setPercent(100)
        }
      })
      await relaunch()
    } catch (e) {
      setError(errorText(e))
      setInstalling(false)
    }
  }

  return (
    <div className="sticky top-0 z-40 border-b border-primary bg-primary/10 px-6 py-3 text-sm text-fg ">
      <div className="mx-auto flex max-w-6xl items-start gap-4">
        <div className="min-w-0 flex-1">
          <div className="font-semibold">发现新版本 v{update.version}</div>
          {update.body && (
            <p className="mt-1 line-clamp-4 text-xs break-words whitespace-pre-wrap text-muted ">
              {update.body}
            </p>
          )}
          {installing && (
            <p className="mt-1 text-xs text-info">
              下载中…{percent === null ? '' : ` ${percent}%`}
            </p>
          )}
          {error && (
            <p className="mt-1 break-all text-xs text-error ">更新失败：{error}</p>
          )}
        </div>
        <div className="flex shrink-0 gap-2">
          <button
            type="button"
            onClick={install}
            disabled={installing}
            className="rounded-lg bg-primary px-3 py-1.5 text-sm text-primary-fg hover:brightness-95 disabled:opacity-50"
          >
            立即更新
          </button>
          <button
            type="button"
            onClick={() => setUpdate(null)}
            disabled={installing}
            className="rounded-lg border border-border px-3 py-1.5 text-sm disabled:opacity-50 "
          >
            稍后
          </button>
        </div>
      </div>
    </div>
  )
}
