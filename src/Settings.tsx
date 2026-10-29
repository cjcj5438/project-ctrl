import { useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  errorText,
  type AiEnv,
  type AiModel,
  type AiSettings,
  type ModelRef,
} from './api'
import { Badge, Button, Field, Input, Segmented, Select, Spinner } from './components'
import { IconActivity, IconRefresh, IconSettings, IconSparkles } from './icons'
import { applyTheme, loadTheme, saveTheme, watchSystemTheme, type ThemeMode } from './theme'

/** 思考档位：与后端 `THINKING_LEVELS` 顺序一致（由弱到强）。 */
const THINKING_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const

const THINKING_TEXT: Record<string, string> = {
  off: '关闭',
  minimal: '最少',
  low: '低',
  medium: '中',
  high: '高',
  xhigh: '很高',
  max: '最高',
}

const thinkingOptions = THINKING_LEVELS.map((v) => ({ value: v, label: `${THINKING_TEXT[v]}（${v}）` }))

const refText = (m: ModelRef | null) => (m ? `${m.provider}/${m.id}` : '')

/** 模型下拉：自动 + 列表；当前值不在列表里也保留（pi 升级/配置手改都可能出现）。 */
function modelOptions(models: AiModel[], current: ModelRef | null) {
  const opts = [
    { value: '', label: '自动（pi 默认）' },
    ...models.map((m) => ({ value: `${m.provider}/${m.id}`, label: `${m.provider} / ${m.name}` })),
  ]
  const cur = refText(current)
  if (cur !== '' && !opts.some((o) => o.value === cur)) {
    opts.push({ value: cur, label: `${cur}（不在当前列表）` })
  }
  return opts
}

const parseRef = (models: AiModel[], value: string): ModelRef | null => {
  if (value === '') return null
  const hit = models.find((m) => `${m.provider}/${m.id}` === value)
  if (hit) return { provider: hit.provider, id: hit.id }
  const at = value.indexOf('/')
  if (at <= 0) return null
  return { provider: value.slice(0, at), id: value.slice(at + 1) }
}

/**
 * 设置页：AI（pi 环境 / 模型 / 思考档位 / 实时进程）+ 外观。
 *
 * `ai_status` 永不报错——pi 没装好会带着 `problem` 回来，这里只把它当文字展示，
 * 页面其它部分照常可用。
 */
export default function Settings() {
  const [env, setEnv] = useState<AiEnv | null>(null)
  const [models, setModels] = useState<AiModel[]>([])
  const [settings, setSettings] = useState<AiSettings | null>(null)
  const [piPath, setPiPath] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [modelsBusy, setModelsBusy] = useState(false)
  /** 「已保存」提示的序号：连续保存也能重新计时。 */
  const [savedTick, setSavedTick] = useState(0)
  const aliveRef = useRef(true)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  useEffect(() => {
    if (savedTick === 0) return
    const timer = window.setTimeout(() => setSavedTick(0), 1500)
    return () => window.clearTimeout(timer)
  }, [savedTick])

  useEffect(() => {
    void (async () => {
      try {
        const e = await api.aiStatus()
        if (!aliveRef.current) return
        setEnv(e)
        setSettings(e.settings)
        setPiPath(e.settings.pi_path ?? '')
        if (!e.problem) {
          try {
            const list = await api.aiModels(false)
            if (aliveRef.current) setModels(list)
          } catch (e2) {
            // 模型列表拿不到不影响设置页：只是下拉里少了候选
            if (aliveRef.current) setError(`模型列表读取失败：${errorText(e2)}`)
          }
        }
      } catch (e) {
        if (aliveRef.current) setError(`读取 AI 设置失败：${errorText(e)}`)
      }
    })()
  }, [])

  /** 落盘：pi 路径输入框的当前值一并写入，再回读 ai_status 刷新环境。 */
  const save = async (next: AiSettings) => {
    setBusy(true)
    try {
      const trimmed = piPath.trim()
      await api.aiSaveSettings({ ...next, pi_path: trimmed === '' ? null : trimmed })
      const e = await api.aiStatus()
      if (!aliveRef.current) return
      setEnv(e)
      setSettings(e.settings)
      setPiPath(e.settings.pi_path ?? '')
      setError(e.problem)
      setSavedTick((n) => n + 1)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    } finally {
      if (aliveRef.current) setBusy(false)
    }
  }

  /** 单个字段改动只更新草稿；「保存」才落盘（避免每敲一下都 fork pi）。 */
  const patch = (p: Partial<AiSettings>) => {
    setSettings((s) => (s ? { ...s, ...p } : s))
    setSavedTick(0)
  }

  const refreshModels = async () => {
    setModelsBusy(true)
    try {
      const list = await api.aiModels(true)
      if (!aliveRef.current) return
      setModels(list)
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(`刷新模型列表失败：${errorText(e)}`)
    } finally {
      if (aliveRef.current) setModelsBusy(false)
    }
  }

  const mainOptions = useMemo(() => modelOptions(models, settings?.main_model ?? null), [models, settings])
  const lightOptions = useMemo(() => modelOptions(models, settings?.light_model ?? null), [models, settings])

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex h-[52px] shrink-0 items-center gap-3 border-b border-border px-4">
        <h1 data-tauri-drag-region className="text-[17px] font-semibold">
          设置
        </h1>
        <div data-tauri-drag-region className="h-full flex-1" />
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="flex max-w-2xl flex-col gap-4">
          <section className="rounded-[var(--radius-card)] border border-border bg-surface-raised p-4">
            <h2 className="flex items-center gap-1.5 text-[13px] font-semibold">
              <IconSparkles size={14} className="text-ai" />
              AI
            </h2>
            {!env && (
              <div className="mt-2 flex flex-col gap-2">
                <div className="flex items-center gap-2 text-[12px] text-muted">
                  <Spinner size={12} />
                  读取中…
                </div>
                {error && (
                  <div className="rounded-[var(--radius-control)] border border-error/40 bg-error/10 px-2 py-1.5 text-[12px] text-error">
                    <span className="break-words">{error}</span>
                  </div>
                )}
              </div>
            )}

            {env && (
              <div className="mt-3 flex flex-col gap-3">
                <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 text-[12px]">
                  <span className="flex items-baseline gap-1.5">
                    <span className="text-muted">pi</span>
                    <span className="font-mono">{env.pi_path ?? '未找到'}</span>
                  </span>
                  <span className="flex items-baseline gap-1.5">
                    <span className="text-muted">版本</span>
                    <span className="font-mono">{env.pi_version ?? '—'}</span>
                  </span>
                  <span className="flex items-center gap-1.5 text-muted">
                    <IconActivity size={11} />
                    运行中会话 {env.live_processes}
                  </span>
                </div>

                {env.problem && (
                  <div className="rounded-[var(--radius-control)] border border-error/40 bg-error/10 px-2 py-1.5 text-[12px] text-error">
                    <span className="break-words">{env.problem}</span>
                  </div>
                )}
                {error && error !== env.problem && (
                  <div className="rounded-[var(--radius-control)] border border-error/40 bg-error/10 px-2 py-1.5 text-[12px] text-error">
                    <span className="break-words">{error}</span>
                  </div>
                )}

                <div className="flex items-end gap-2">
                  <div className="min-w-0 flex-1">
                    <Field label="pi 可执行文件路径（留空则按 PATH 查找）">
                      <Input
                        value={piPath}
                        placeholder="/usr/local/bin/pi"
                        spellCheck={false}
                        onChange={(e) => setPiPath(e.target.value)}
                      />
                    </Field>
                  </div>
                  <Button onClick={() => settings && void save(settings)} disabled={busy || !settings}>
                    检测
                  </Button>
                </div>

                {settings && (
                  <>
                    <div className="grid gap-2 [grid-template-columns:repeat(auto-fit,minmax(180px,1fr))]">
                      <Field label="主模型（配置 / 诊断 / README）">
                        <Select
                          className="w-full"
                          value={refText(settings.main_model)}
                          options={mainOptions}
                          onChange={(v) => patch({ main_model: parseRef(models, v) })}
                        />
                      </Field>
                      <Field label="主模型思考档位">
                        <Select
                          className="w-full"
                          value={settings.main_thinking}
                          options={thinkingOptions}
                          onChange={(v) => patch({ main_thinking: v })}
                        />
                      </Field>
                      <Field label="轻量模型（体检 / 画像）">
                        <Select
                          className="w-full"
                          value={refText(settings.light_model)}
                          options={lightOptions}
                          onChange={(v) => patch({ light_model: parseRef(models, v) })}
                        />
                      </Field>
                      <Field label="轻量模型思考档位">
                        <Select
                          className="w-full"
                          value={settings.light_thinking}
                          options={thinkingOptions}
                          onChange={(v) => patch({ light_thinking: v })}
                        />
                      </Field>
                    </div>

                    <div className="flex flex-wrap items-center gap-2">
                      <Button variant="primary" disabled={busy} onClick={() => void save(settings)}>
                        保存
                      </Button>
                      <Button onClick={() => void refreshModels()} disabled={modelsBusy || !!env.problem}>
                        {modelsBusy ? <Spinner size={11} /> : <IconRefresh size={12} />}
                        刷新模型列表
                      </Button>
                      {models.length > 0 && (
                        <Badge tone="neutral">{models.length} 个可用模型</Badge>
                      )}
                      {savedTick > 0 && <span className="text-[12px] text-success">已保存</span>}
                    </div>
                  </>
                )}

                <p className="flex items-center gap-1.5 text-[11px] text-subtle">
                  <IconSettings size={11} />
                  AI 全程只读，不会修改项目代码
                </p>
              </div>
            )}
          </section>

          <section className="rounded-[var(--radius-card)] border border-border bg-surface-raised p-4">
            <h2 className="text-[13px] font-semibold">外观</h2>
            <div className="mt-3">
              <ThemePicker />
            </div>
          </section>
        </div>
      </div>
    </div>
  )
}

/** 主题三档：与侧栏同一套持久化（localStorage + html.dark）。 */
function ThemePicker() {
  const [mode, setMode] = useState<ThemeMode>(() => loadTheme())

  useEffect(() => {
    applyTheme(mode)
    saveTheme(mode)
  }, [mode])

  useEffect(() => {
    if (mode !== 'auto') return
    return watchSystemTheme(() => applyTheme('auto'))
  }, [mode])

  return (
    <Segmented
      value={mode}
      onChange={setMode}
      items={[
        { id: 'auto', label: '自动' },
        { id: 'light', label: '浅色' },
        { id: 'dark', label: '深色' },
      ]}
    />
  )
}
