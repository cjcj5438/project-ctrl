import type { ButtonHTMLAttributes, InputHTMLAttributes, ReactNode, TextareaHTMLAttributes } from 'react'

/**
 * 原子组件层：全站唯一的按钮/输入框/徽章/卡片外观来源。
 *
 * 之前每个文件内联复制同一串 Tailwind class，改一处颜色要改 20 个文件；现在只有这里定义，
 * 且颜色全部走 `index.css` 的语义 token（`surface` / `fg` / `border` / `accent` …），
 * 亮暗两套由 `.dark` 类切换，不再散落 `dark:` 变体。
 */

type Variant = 'primary' | 'outline' | 'ghost' | 'danger'
type Size = 'sm' | 'md'

const VARIANT: Record<Variant, string> = {
  primary:
    'bg-primary text-primary-fg hover:brightness-110 disabled:opacity-40 border border-transparent',
  outline:
    'border border-border bg-surface text-fg hover:bg-surface-raised disabled:opacity-40',
  ghost: 'border border-transparent text-muted hover:bg-surface-raised disabled:opacity-40',
  danger: 'border border-error/50 text-error hover:brightness-110/10 disabled:opacity-40',
}

const SIZE: Record<Size, string> = {
  // 触控目标不小于 28px；小按钮只用在密集表格行内
  sm: 'px-2 py-1 text-xs rounded-md',
  md: 'px-3 py-1.5 text-sm rounded-lg',
}

export function Button({
  variant = 'outline',
  size = 'md',
  className = '',
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: Size }) {
  return (
    <button
      {...rest}
      className={`inline-flex items-center gap-1.5 whitespace-nowrap transition-colors disabled:cursor-not-allowed ${VARIANT[variant]} ${SIZE[size]} ${className}`}
    />
  )
}

export function IconButton({
  label,
  className = '',
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return (
    <button
      {...rest}
      aria-label={label}
      title={label}
      className={`inline-flex h-7 w-7 items-center justify-center rounded-md border border-transparent text-muted transition-colors hover:bg-surface-raised hover:text-fg disabled:opacity-40 ${className}`}
    >
      {children}
    </button>
  )
}

const FIELD =
  'w-full rounded-md border border-border bg-surface px-2 py-1 text-sm text-fg placeholder:text-muted/70 focus:border-accent'

export function Input({ className = '', ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...rest} className={`${FIELD} ${className}`} />
}

export function TextArea({ className = '', ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea {...rest} className={`${FIELD} font-mono text-xs ${className}`} />
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-0.5 block text-[11px] text-muted">{label}</span>
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
  tone?: 'neutral' | 'accent' | 'success' | 'warning' | 'error'
  title?: string
}) {
  const tones: Record<string, string> = {
    neutral: 'border-border text-muted',
    accent: 'border-accent/50 text-accent',
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
}: React.HTMLAttributes<HTMLDivElement> & { children: ReactNode }) {
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
      <summary className="cursor-pointer px-3 py-2 text-sm font-semibold">{title}</summary>
      <div className="border-t border-border p-3">{children}</div>
    </details>
  )
}

export function ErrorBanner({ text, onClose }: { text: string; onClose?: () => void }) {
  return (
    <div className="mt-3 flex items-start gap-2 rounded-lg border border-error/40 bg-error/10 px-3 py-2 text-sm text-error">
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{text}</span>
      {onClose && (
        <button onClick={onClose} className="shrink-0 text-xs underline" aria-label="关闭提示">
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
  return (
    <div
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/50 p-6"
      role="dialog"
      aria-modal="true"
      aria-label={typeof title === 'string' ? title : '对话框'}
      onMouseDown={(e) => {
        // 点遮罩关闭（点内容不关）
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className={`flex max-h-[85vh] w-full flex-col rounded-xl border border-border bg-surface ${width}`}>
        <div className="flex items-center gap-3 border-b border-border px-4 py-2.5">
          <h2 className="text-sm font-semibold">{title}</h2>
          <button
            onClick={onClose}
            className="ml-auto rounded-md px-2 py-0.5 text-xs text-muted hover:bg-surface-raised hover:text-fg"
            aria-label="关闭"
          >
            关闭
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-auto p-4">{children}</div>
        {footer && <div className="flex items-center gap-2 border-t border-border px-4 py-2.5">{footer}</div>}
      </div>
    </div>
  )
}

/** 运行状态圆点：颜色不是唯一信息源（旁边总带文字标签）。 */
export function StatusDot({ tone, label }: { tone: 'idle' | 'busy' | 'ok' | 'bad'; label?: string }) {
  const map = {
    idle: 'bg-muted/50',
    busy: 'bg-warning',
    ok: 'bg-success',
    bad: 'bg-error',
  } as const
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className={`inline-block h-2 w-2 shrink-0 rounded-full ${map[tone]}`} aria-hidden />
      {label && <span className="text-xs">{label}</span>}
    </span>
  )
}
