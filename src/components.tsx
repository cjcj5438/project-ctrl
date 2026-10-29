import { cloneElement, isValidElement, useEffect, useRef, useState } from 'react'
import type {
  ButtonHTMLAttributes,
  HTMLAttributes,
  InputHTMLAttributes,
  MouseEvent as ReactMouseEvent,
  ReactElement,
  ReactNode,
  TextareaHTMLAttributes,
} from 'react'

/** Menu 触发器需要转发的事件属性（其余属性原样保留）。 */
type TriggerProps = {
  onClick?: (e: ReactMouseEvent) => void
  'aria-haspopup'?: string
  'aria-expanded'?: boolean
}

/**
 * 原子组件层：全站唯一的按钮/输入框/徽章/卡片外观来源。
 *
 * 之前每个文件内联复制同一串 Tailwind class，改一处颜色要改 20 个文件；现在只有这里定义，
 * 且颜色全部走 `index.css` 的语义 token（`surface` / `fg` / `border` / `primary` …），
 * 亮暗两套由 `.dark` 类切换，不再散落 `dark:` 变体。
 *
 * 尺寸约定：控件高度 24/28px，圆角走 --radius-control / --radius-card / --radius-panel；
 * 阴影只给浮层（Modal / Menu），页面层次靠底色与边框。
 */

type Variant = 'primary' | 'outline' | 'ghost' | 'danger'
type Size = 'sm' | 'md'

const VARIANT: Record<Variant, string> = {
  primary:
    'bg-primary text-primary-fg hover:brightness-110 active:brightness-95 border border-transparent',
  outline:
    'border border-border-strong bg-surface text-fg hover:bg-surface-raised',
  ghost: 'border border-transparent text-muted hover:bg-surface-raised hover:text-fg',
  danger: 'border border-error/50 text-error hover:bg-error/10',
}

const SIZE: Record<Size, string> = {
  // 触控目标：sm 24px / md 28px；sm 只用在密集表格行内
  sm: 'h-6 px-2 text-[12px] gap-1',
  md: 'h-7 px-2.5 text-[13px] gap-1.5',
}

const FOCUS = 'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/70'

export function Button({
  variant = 'outline',
  size = 'md',
  type = 'button',
  className = '',
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: Size }) {
  return (
    <button
      type={type}
      {...rest}
      className={`inline-flex shrink-0 items-center justify-center whitespace-nowrap rounded-[var(--radius-control)] transition-colors duration-150 ease-out disabled:cursor-not-allowed disabled:opacity-50 ${FOCUS} ${VARIANT[variant]} ${SIZE[size]} ${className}`}
    />
  )
}

export function IconButton({
  label,
  type = 'button',
  className = '',
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return (
    <button
      type={type}
      {...rest}
      aria-label={label}
      title={label}
      className={`inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-[var(--radius-control)] border border-transparent text-muted transition-colors duration-150 ease-out hover:bg-surface-raised hover:text-fg disabled:cursor-not-allowed disabled:opacity-50 ${FOCUS} ${className}`}
    >
      {children}
    </button>
  )
}

const FIELD =
  'w-full rounded-[var(--radius-control)] border border-border bg-surface px-2 text-[13px] text-fg placeholder:text-subtle focus:border-primary focus-visible:outline-none'

export function Input({ className = '', ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...rest} className={`${FIELD} h-7 ${className}`} />
}

export function TextArea({ className = '', ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea {...rest} className={`${FIELD} py-1.5 font-mono text-[12px] ${className}`} />
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-[11px] text-muted">{label}</span>
      {children}
    </label>
  )
}

export function Badge({
  children,
  tone = 'neutral',
  title,
}: {
  children: ReactNode
  tone?: 'neutral' | 'success' | 'warning' | 'error'
  title?: string
}) {
  const tones: Record<string, string> = {
    neutral: 'border-border text-muted',
    success: 'border-success/50 text-success',
    warning: 'border-warning/50 text-warning',
    error: 'border-error/50 text-error',
  }
  return (
    <span
      title={title}
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-[11px] ${tones[tone]}`}
    >
      {children}
    </span>
  )
}

export function Card({
  children,
  className = '',
  ...rest
}: HTMLAttributes<HTMLDivElement> & { children: ReactNode }) {
  return (
    <div
      {...rest}
      className={`rounded-[var(--radius-card)] border border-border bg-surface ${className}`}
    >
      {children}
    </div>
  )
}

export function Panel({ title, children }: { title: ReactNode; children: ReactNode }) {
  return (
    <details className="mt-4 rounded-[var(--radius-card)] border border-border">
      <summary className="cursor-pointer px-3 py-2 text-[13px] font-semibold">{title}</summary>
      <div className="border-t border-border p-3">{children}</div>
    </details>
  )
}

export function ErrorBanner({ text, onClose }: { text: string; onClose?: () => void }) {
  return (
    <div className="mt-3 flex items-start gap-2 rounded-[var(--radius-card)] border border-error/40 bg-error/10 px-3 py-2 text-[13px] text-error">
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{text}</span>
      {onClose && (
        <button
          type="button"
          onClick={onClose}
          className={`shrink-0 rounded text-[12px] underline ${FOCUS}`}
          aria-label="关闭提示"
        >
          关闭
        </button>
      )}
    </div>
  )
}

export function Modal({
  title,
  children,
  onClose,
  width = 'max-w-2xl',
  footer,
}: {
  title: ReactNode
  children: ReactNode
  onClose: () => void
  width?: string
  footer?: ReactNode
}) {
  // Esc 关闭：监听 window 而不是容器，焦点在内部任意控件上都能生效
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        onClose()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return (
    <div
      className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6"
      role="dialog"
      aria-modal="true"
      aria-label={typeof title === 'string' ? title : '对话框'}
      tabIndex={-1}
      onMouseDown={(e) => {
        // 点遮罩关闭（点内容不关）
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div
        className={`flex max-h-[85vh] w-full flex-col overflow-hidden rounded-[var(--radius-panel)] border border-border-strong bg-surface shadow-overlay ${width}`}
      >
        <div className="flex items-center gap-3 border-b border-border px-4 py-2.5">
          <h2 className="text-[13px] font-semibold">{title}</h2>
          <button
            type="button"
            onClick={onClose}
            className={`ml-auto rounded-[var(--radius-control)] px-2 py-0.5 text-[12px] text-muted transition-colors hover:bg-surface-raised hover:text-fg ${FOCUS}`}
            aria-label="关闭"
          >
            关闭
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-auto p-4">{children}</div>
        {footer && (
          <div className="flex items-center gap-2 border-t border-border px-4 py-2.5">{footer}</div>
        )}
      </div>
    </div>
  )
}

/** 运行状态圆点：颜色不是唯一信息源（旁边总带文字标签）。 */
export function StatusDot({
  tone,
  label,
  title,
}: {
  tone: 'idle' | 'busy' | 'ok' | 'bad'
  label?: string
  title?: string
}) {
  const map = {
    idle: 'bg-muted/50',
    busy: 'bg-warning',
    ok: 'bg-success',
    bad: 'bg-error-solid',
  } as const
  return (
    <span
      className="inline-flex items-center gap-1.5"
      title={title}
      role={title && !label ? 'img' : undefined}
      aria-label={title && !label ? title : undefined}
    >
      <span className={`inline-block h-2 w-2 shrink-0 rounded-full ${map[tone]}`} aria-hidden />
      {label && <span className="text-[12px]">{label}</span>}
    </span>
  )
}

/** 下划线页签：溢出横向滚动，不换行（窗口可以窄到 1280）。 */
export function Tabs({
  items,
  value,
  onChange,
}: {
  items: { id: string; label: string; badge?: string | number }[]
  value: string
  onChange: (id: string) => void
}) {
  return (
    <div role="tablist" className="flex min-w-0 items-center gap-4 overflow-x-auto">
      {items.map((it) => {
        const active = it.id === value
        return (
          <button
            key={it.id}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(it.id)}
            className={`-mb-px flex shrink-0 items-center gap-1.5 border-b-2 px-0.5 pb-1.5 pt-1 text-[13px] transition-colors duration-150 ease-out ${FOCUS} ${
              active
                ? 'border-primary text-fg'
                : 'border-transparent text-muted hover:text-fg'
            }`}
          >
            {it.label}
            {it.badge !== undefined && (
              <span
                className={`rounded-full px-1.5 text-[11px] tabular ${
                  active ? 'bg-primary/15 text-primary' : 'bg-surface-sunken text-muted'
                }`}
              >
                {it.badge}
              </span>
            )}
          </button>
        )
      })}
    </div>
  )
}

/** 分段控件：内嵌槽 + 选中胶囊；用于同层级视图切换。 */
export function Segmented<T extends string>({
  items,
  value,
  onChange,
}: {
  items: { id: T; label: string }[]
  value: T
  onChange: (v: T) => void
}) {
  return (
    <div
      role="tablist"
      className="inline-flex items-center gap-0.5 rounded-[var(--radius-control)] bg-surface-sunken p-0.5"
    >
      {items.map((it) => {
        const active = it.id === value
        return (
          <button
            key={it.id}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(it.id)}
            className={`h-6 shrink-0 rounded-[4px] border px-2.5 text-[12px] transition-colors duration-150 ease-out ${FOCUS} ${
              active
                ? 'border-border bg-surface-raised text-fg'
                : 'border-transparent text-muted hover:text-fg'
            }`}
          >
            {it.label}
          </button>
        )
      })}
    </div>
  )
}

/** 下拉菜单：浮层阴影、点外部 / Esc 关闭，条目是按钮（可键盘触发）。 */
export function Menu({
  trigger,
  items,
  align = 'start',
}: {
  trigger: ReactNode
  items: { label: string; icon?: ReactNode; onSelect: () => void; danger?: boolean }[]
  align?: 'start' | 'end'
}) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const first = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    if (!open) return
    // mousedown 而不是 click：外部点击先关，避免和条目 click 抢事件
    const onDown = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    first.current?.focus()
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
      // 关闭后焦点回到触发器，键盘用户不会掉到 body
      root.current?.querySelector<HTMLElement>('[aria-haspopup="menu"]')?.focus()
    }
  }, [open])

  return (
    <div ref={root} className="relative inline-flex">
      {isValidElement(trigger) ? (
        cloneElement(
          trigger as ReactElement<TriggerProps>,
          {
            'aria-haspopup': 'menu',
            'aria-expanded': open,
            onClick: (e: ReactMouseEvent) => {
              // 触发器自身的 onClick 先跑（比如外部埋点），再切换开合
              (trigger.props as TriggerProps).onClick?.(e)
              setOpen((v) => !v)
            },
          },
        )
      ) : (
        // trigger 不是元素时兜底成按钮，保证键盘可达
        <Button
          variant="ghost"
          size="sm"
          aria-haspopup="menu"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
        >
          {trigger}
        </Button>
      )}
      {open && (
        <div
          role="menu"
          className={`absolute top-full z-40 mt-1 min-w-44 rounded-[var(--radius-card)] border border-border-strong bg-surface-raised p-1 shadow-overlay ${
            align === 'end' ? 'right-0' : 'left-0'
          }`}
        >
          {items.map((it, i) => (
            <button
              key={`${it.label}-${i}`}
              ref={i === 0 ? first : undefined}
              type="button"
              role="menuitem"
              onClick={() => {
                setOpen(false)
                it.onSelect()
              }}
              className={`flex w-full items-center gap-2 rounded-[4px] px-2 py-1.5 text-left text-[12px] transition-colors duration-150 ease-out ${FOCUS} ${
                it.danger ? 'text-error hover:bg-error/10' : 'text-fg hover:bg-surface'
              }`}
            >
              {it.icon && <span className="shrink-0 text-muted">{it.icon}</span>}
              <span className="min-w-0 flex-1 truncate">{it.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

/** 开关：32×18 轨道 + 14px 旋钮；Space/Enter 由 button 原生触发。 */
export function Switch({
  checked,
  onChange,
  disabled = false,
  label,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  disabled?: boolean
  label?: string
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`inline-flex h-[18px] w-8 shrink-0 items-center rounded-full border transition-colors duration-150 ease-out disabled:cursor-not-allowed disabled:opacity-50 ${FOCUS} ${
        checked ? 'border-primary bg-primary' : 'border-border-strong bg-surface-sunken'
      }`}
    >
      <span
        aria-hidden
        className={`h-3.5 w-3.5 rounded-full border border-border-strong bg-primary-fg transition-transform duration-150 ease-out ${
          checked ? 'translate-x-[14px]' : 'translate-x-[2px]'
        }`}
      />
    </button>
  )
}

/** 原生 select，外观交给 token：28px 高、无系统箭头、右侧自绘 chevron。 */
export function Select({
  value,
  options,
  onChange,
  placeholder,
  disabled = false,
  className = '',
}: {
  value: string
  options: { value: string; label: string }[]
  onChange: (v: string) => void
  placeholder?: string
  disabled?: boolean
  className?: string
}) {
  return (
    <div className={`relative inline-flex ${className}`}>
      <select
        value={value}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
        className={`h-7 w-full appearance-none rounded-[var(--radius-control)] border border-border bg-surface pl-2 pr-6 text-[13px] text-fg transition-colors duration-150 ease-out disabled:cursor-not-allowed disabled:opacity-50 ${FOCUS}`}
      >
        {placeholder !== undefined && (
          <option value="" disabled>
            {placeholder}
          </option>
        )}
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
      <svg
        aria-hidden
        viewBox="0 0 16 16"
        width="12"
        height="12"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="pointer-events-none absolute right-1.5 top-1/2 -translate-y-1/2 text-muted"
      >
        <path d="M4 6.5 8 10.5l4-4" />
      </svg>
    </div>
  )
}

/** 纯 CSS 旋转环：减少动效时由全局媒体查询停掉。 */
export function Spinner({ size = 14 }: { size?: number }) {
  return (
    <span role="status" aria-label="加载中" className="inline-flex">
      <span
        aria-hidden
        className="animate-spin rounded-full border-2 border-border-strong border-t-primary"
        style={{ width: size, height: size }}
      />
    </span>
  )
}

/** 空状态：图标 + 标题 + 提示 + 可选操作，居中弱化。 */
export function EmptyState({
  icon,
  title,
  hint,
  action,
}: {
  icon?: ReactNode
  title: string
  hint?: string
  action?: ReactNode
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-6 py-10 text-center">
      {icon && <span className="text-subtle">{icon}</span>}
      <div className="text-[13px] text-fg">{title}</div>
      {hint && <div className="max-w-sm text-[12px] text-muted">{hint}</div>}
      {action && <div className="mt-1">{action}</div>}
    </div>
  )
}
