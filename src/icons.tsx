/**
 * 图标：内联 SVG（16px 网格，currentColor）。
 *
 * 不用 emoji、也不用 Unicode 字符（`✎` `✕` `×` `←` 在中英混排与等宽字体里宽度不一致，
 * 会让按钮看起来歪），统一用 SVG 后对齐与缩放才是确定的。
 */
type P = { size?: number; className?: string }

const base = (size: number, className?: string) => ({
  width: size,
  height: size,
  viewBox: '0 0 16 16',
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 1.5,
  strokeLinecap: 'round' as const,
  strokeLinejoin: 'round' as const,
  className,
  'aria-hidden': true,
})

export const IconChevronLeft = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M10 3 5 8l5 5" />
  </svg>
)

export const IconPencil = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M11.3 2.2 13.8 4.7 5.6 12.9 2.5 13.5l.6-3.1z" />
  </svg>
)

export const IconX = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M4 4l8 8M12 4l-8 8" />
  </svg>
)

export const IconPlay = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M5 3.5v9l7-4.5z" />
  </svg>
)

export const IconStop = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="4.5" y="4.5" width="7" height="7" rx="1" />
  </svg>
)

export const IconRefresh = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M13 8a5 5 0 1 1-1.6-3.7M13 2.5V5h-2.5" />
  </svg>
)

export const IconFolder = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M2 4.5h4l1.5 2H14v6H2z" />
  </svg>
)

export const IconTerminal = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M5 6.5 7 8l-2 1.5M8.5 10h2.5" />
  </svg>
)

export const IconCode = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M6 4 2.5 8 6 12M10 4l3.5 4L10 12" />
  </svg>
)

export const IconGithub = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M6 13.5c-3 1-3-1.5-3-1.5M11 13.5V11c0-1-.5-1.5-1-2 2-.3 3-1.5 3-3.5 0-1-.3-1.8-1-2.5.3-1 .3-2-.1-2.5 0 0-1 .1-2 1a6 6 0 0 0-3 0c-1-.9-2-1-2-1-.4.5-.4 1.5-.1 2.5C4.3 4.7 4 5.5 4 6.5c0 2 1 3.2 3 3.5-.4.4-.5.9-.5 1.5v2" />
  </svg>
)

export const IconPlus = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M8 3.5v9M3.5 8h9" />
  </svg>
)

export const IconSearch = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <circle cx="7" cy="7" r="4.2" />
    <path d="M10.2 10.2 13.5 13.5" />
  </svg>
)

export const IconMoon = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M13 9.5A5.5 5.5 0 0 1 6.5 3a5.5 5.5 0 1 0 6.5 6.5z" />
  </svg>
)

export const IconSun = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <circle cx="8" cy="8" r="3" />
    <path d="M8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1 1M11.6 11.6l1 1M12.6 3.4l-1 1M4.4 11.6l-1 1" />
  </svg>
)

export const IconMonitor = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2" y="3" width="12" height="8" rx="1" />
    <path d="M5.5 13.5h5M8 11v2.5" />
  </svg>
)

export const IconDocker = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2" y="7" width="3" height="3" />
    <rect x="5.5" y="7" width="3" height="3" />
    <rect x="5.5" y="3.8" width="3" height="3" />
    <path d="M9 7h3v3H9zM2 10c0 2 1.5 3 4 3h3c2.5 0 4-1.5 4-3.5" />
  </svg>
)

export const IconExternal = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M9 3h4v4M13 3 7.5 8.5M11 9.5V12H4V5h2.5" />
  </svg>
)

export const IconSparkles = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M6.5 2.5 7.6 5.4 10.5 6.5 7.6 7.6 6.5 10.5 5.4 7.6 2.5 6.5 5.4 5.4z" />
    <path d="M11.5 9.5l.6 1.4 1.4.6-1.4.6-.6 1.4-.6-1.4L9.5 12l1.4-.6z" />
  </svg>
)

export const IconSettings = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <circle cx="8" cy="8" r="2.2" />
    <path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M12.4 3.6l-1.1 1.1M4.7 11.3l-1.1 1.1" />
  </svg>
)

export const IconServer = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2" y="2.5" width="12" height="4.5" rx="1" />
    <rect x="2" y="9" width="12" height="4.5" rx="1" />
    <path d="M4.5 4.75h1M4.5 11.25h1" />
  </svg>
)

export const IconGrid = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2.5" y="2.5" width="4.5" height="4.5" rx="1" />
    <rect x="9" y="2.5" width="4.5" height="4.5" rx="1" />
    <rect x="2.5" y="9" width="4.5" height="4.5" rx="1" />
    <rect x="9" y="9" width="4.5" height="4.5" rx="1" />
  </svg>
)

export const IconGlobe = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <circle cx="8" cy="8" r="5.5" />
    <path d="M2.5 8h11M8 2.5c1.6 1.8 2.4 3.6 2.4 5.5S9.6 11.7 8 13.5C6.4 11.7 5.6 9.9 5.6 8S6.4 4.3 8 2.5z" />
  </svg>
)

export const IconStethoscope = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M3 2.5v3a2.5 2.5 0 0 0 5 0v-3" />
    <path d="M5.5 8v2.5A3 3 0 0 0 8.5 13.5h1.5" />
    <circle cx="12" cy="12" r="1.75" />
  </svg>
)

export const IconActivity = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M1.5 8.5h3l2-5 3 9 2-4h3" />
  </svg>
)

export const IconIdCard = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2" y="3.5" width="12" height="9" rx="1.5" />
    <circle cx="5.8" cy="7.3" r="1.3" />
    <path d="M4 11c.3-1 1-1.5 1.8-1.5S7.5 10 7.8 11M9.5 6.5h3M9.5 9h2.2" />
  </svg>
)

export const IconFileText = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M4 2.5h5l3 3v8H4z" />
    <path d="M9 2.5V6h3M5.8 8.5h4.4M5.8 10.8h3.2" />
  </svg>
)

export const IconWrench = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M10.2 2.3a3.6 3.6 0 0 1 3.4 4.9L8.4 12a2.2 2.2 0 1 1-3.1-3.1l4.8-5.2a3.6 3.6 0 0 1 .1-1.4z" />
    <path d="M3.4 12.6 2.2 13.8" />
  </svg>
)

export const IconSend = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M2.5 8 13.5 2.8 10.4 13.5 8 9.5z" />
    <path d="M8 9.5 13.5 2.8" />
  </svg>
)

export const IconSquare = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="3.5" y="3.5" width="9" height="9" rx="1.5" />
  </svg>
)

export const IconChevronRight = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M6 3l5 5-5 5" />
  </svg>
)

export const IconCopy = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" />
    <path d="M10.5 5.5v-2h-8v8h2" />
  </svg>
)

export const IconCheck = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M3 8.5 6.5 12 13 4.5" />
  </svg>
)

export const IconAlertTriangle = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M8 2.5 14 13H2z" />
    <path d="M8 6.5v3.2M8 11.5v.01" />
  </svg>
)

export const IconPanelRight = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M9.5 3v10" />
  </svg>
)

export const IconMoreHorizontal = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M4 8h.01M8 8h.01M12 8h.01" />
  </svg>
)

export const IconTrash = ({ size = 16, className }: P) => (
  <svg {...base(size, className)}>
    <path d="M2.5 4.5h11M6.5 4.5V2.8h3v1.7M4 4.5l.7 8.2h6.6L12 4.5" />
    <path d="M6.5 7v3.5M9.5 7v3.5" />
  </svg>
)
