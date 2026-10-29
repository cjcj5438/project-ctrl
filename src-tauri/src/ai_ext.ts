/**
 * project-ctrl × pi 编码代理扩展（用 `pi -e src-tauri/src/ai_ext.ts` 加载）。
 *
 * 只做三件事：
 * 1. 每次工具调用先送进 app 的权限闸门（`pc-gate` 通道）；闸门给了 patch 就原地改写入参；
 * 2. 注册 8 个 `pc_*` 工具，把结构化结果交回 app（`pc-host` 通道），正文里不再粘贴整份 JSON；
 * 3. 服务配方（ServiceSpec）的入参在这里就对着 Rust 侧 `deny_unknown_fields` 收紧，
 *    未知字段在模型这一侧就被挡住，而不是等落盘时才发现。
 *
 * 两条通道的形状一致：`ctx.ui.input(通道名, JSON.stringify({v:1,…}), { timeout })`。
 * 返回 undefined 表示用户取消或宿主超时——两者对扩展而言都是「没有拿到答案」。
 */

import { defineTool, type ExtensionAPI, type ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

/** 权限闸门通道名。 */
const GATE_TITLE = "pc-gate";
/** 宿主调用通道名。 */
const HOST_TITLE = "pc-host";
/** 闸门必须很快回来，超时即视为拒绝。 */
const GATE_TIMEOUT_MS = 10_000;

/** `pc-host` 的回包。 */
interface HostReply {
  ok?: boolean;
  result?: unknown;
  error?: unknown;
}

/** `pc-gate` 的回包：`patch` 是对象时合并进工具入参。 */
interface GateReply {
  allow?: boolean;
  reason?: unknown;
  patch?: unknown;
}

/** 取出纯对象，数组与 null 都不算。 */
function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

/** 把宿主结果转成给模型看的文本：永远是合法 JSON，undefined 记为 null。 */
function jsonText(value: unknown): string {
  const text = JSON.stringify(value);
  return text === undefined ? "null" : text;
}

/**
 * 调一次宿主方法。
 *
 * - 取消/超时 → 抛「已取消」或「app 未响应（超时）」；
 * - 回包不是 JSON、`ok !== true` → 抛宿主给的中文错误（或兜底文案）。
 */
async function hostCall<T>(
  ctx: ExtensionContext,
  method: string,
  params: unknown,
  opts: { timeout: number; signal?: AbortSignal },
): Promise<T> {
  const raw = await ctx.ui.input(HOST_TITLE, JSON.stringify({ v: 1, method, params }), {
    timeout: opts.timeout,
    signal: opts.signal,
  });
  if (raw === undefined) {
    throw new Error(opts.signal?.aborted ? "已取消" : "app 未响应（超时）");
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new Error("app 返回的不是合法 JSON");
  }

  const reply = asRecord(parsed) as HostReply | undefined;
  if (reply === undefined || reply.ok !== true) {
    const error = reply?.error;
    throw new Error(typeof error === "string" && error.length > 0 ? error : "host 调用失败");
  }
  return reply.result as T;
}

/* ------------------------------------------------------------------------- *
 * ServiceSpec：与 src-tauri/src/services.rs 的 serde 形状逐字段对齐。
 *
 * 全部对象都是 `additionalProperties: false`（Rust 侧是 `deny_unknown_fields`）；
 * 可省略的字段一律 `Type.Optional`，且不给 `default`——默认值会把字段塞进 JSON，
 * 变成 Rust 眼里的「多余字段」。唯一例外是 `env`：它是键值映射，键本身是任意的。
 * ------------------------------------------------------------------------- */

/** 一次性步骤：安装 / 迁移 / 构建。 */
const StepSchema = Type.Object(
  {
    name: Type.String({ minLength: 1, description: "步骤名，服务内唯一" }),
    argv: Type.Array(Type.String(), {
      minItems: 1,
      description: "命令的 argv 数组，如 [\"pnpm\",\"install\"]；不要写成一整条 shell 字符串",
    }),
    timeout_s: Type.Optional(
      Type.Integer({ minimum: 0, description: "单步超时（秒），省略时为 1800" }),
    ),
  },
  { additionalProperties: false },
);

/** 健康检查：port 只需端口可连，http 需要 2xx/3xx。 */
const HealthSchema = Type.Object(
  {
    type: Type.Union([Type.Literal("port"), Type.Literal("http")], {
      description: "port = 端口可连即就绪；http = HTTP GET 返回 2xx/3xx 即就绪",
    }),
    path: Type.Optional(
      Type.String({
        pattern: "^/\\S*$",
        description: "http 检查路径，必须以 / 开头且不含空白，如 /health",
      }),
    ),
    timeout_ms: Type.Optional(
      Type.Integer({ minimum: 100, maximum: 120_000, description: "就绪等待上限（毫秒），100–120000" }),
    ),
  },
  { additionalProperties: false },
);

/** 端口：listen 是本服务监听的端口，requires 是必须已在监听的依赖端口。 */
const PortsSchema = Type.Object(
  {
    listen: Type.Optional(
      Type.Integer({ minimum: 1, maximum: 65_535, description: "本服务监听的端口，不能为 0" }),
    ),
    requires: Type.Optional(
      Type.Array(Type.Integer({ minimum: 0, maximum: 65_535 }), {
        description: "必须已被占用的端口列表",
      }),
    ),
  },
  { additionalProperties: false },
);

/** docker compose：多个 -f 叠加，只操作列出的子服务。 */
const DockerSchema = Type.Object(
  {
    files: Type.Array(Type.String(), {
      minItems: 1,
      description: "compose 文件，项目内相对路径（可多个，按顺序叠加）",
    }),
    project_name: Type.Optional(Type.String({ description: "compose project name，省略则由 compose 推断" })),
    services: Type.Optional(
      Type.Array(Type.String(), { description: "要操作的子服务名；有 run 时可省略" }),
    ),
  },
  { additionalProperties: false },
);

/** 一个服务的完整声明。 */
const ServiceSpecSchema = Type.Object(
  {
    name: Type.String({ minLength: 1, description: "服务名，不含路径分隔符，depends_on 用它引用" }),
    cwd: Type.Optional(
      Type.String({ description: "启动目录，项目内相对路径（monorepo 子包），如 apps/web" }),
    ),
    run: Type.Optional(
      Type.Array(Type.String(), {
        minItems: 1,
        description:
          "长驻命令的 argv 数组。需要 shell 特性就显式写 [\"/bin/zsh\",\"-lc\",\"…\"]；纯步骤服务整段省略",
      }),
    ),
    steps: Type.Optional(Type.Array(StepSchema, { description: "一次性步骤（安装/构建/迁移），默认不随启动执行" })),
    env_files: Type.Optional(
      Type.Array(Type.String(), { description: "env 文件名，项目内相对路径，只记名字不记值" }),
    ),
    env: Type.Optional(
      Type.Record(Type.String(), Type.String(), {
        additionalProperties: false,
        description: "追加的环境变量（键 → 值）",
      }),
    ),
    ports: Type.Optional(PortsSchema),
    health: Type.Optional(HealthSchema),
    depends_on: Type.Optional(
      Type.Array(Type.String(), { description: "依赖的服务名（本配置里的 name），不能指向自己" }),
    ),
    restart: Type.Optional(
      Type.Union([Type.Literal("never"), Type.Literal("on-failure"), Type.Literal("always")], {
        description: "退出后的重启策略：never / on-failure（默认）/ always",
      }),
    ),
    docker: Type.Optional(DockerSchema),
  },
  { additionalProperties: false },
);

/* ------------------------------------------------------------------------- *
 * 8 个工具。timeout 按各工具的真实耗时给：日志/上下文秒级，
 * 提交配方与启停服务可能要等到人点确认，所以给到分钟级。
 * ------------------------------------------------------------------------- */

const projectContextTool = defineTool({
  name: "pc_project_context",
  label: "项目上下文",
  description:
    "读取当前项目的事实：目录结构、已声明的服务、端口与环境变量名（不含变量值）。回答任何项目相关问题前先调用一次。",
  promptSnippet: "读取项目结构、服务清单、端口与可用环境变量名（不含值）",
  promptGuidelines: [
    "在回答任何关于本项目的问题之前，先调用 pc_project_context 获取事实，不要凭猜测描述项目结构或服务。",
    "需要环境变量时只从 pc_project_context 拿变量名，不要读 .env 文件。",
  ],
  parameters: Type.Object({}, { additionalProperties: false }),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "context", params, { timeout: 30_000, signal });
    return { content: [{ type: "text", text: jsonText(result) }], details: result };
  },
});

const serviceLogsTool = defineTool({
  name: "pc_service_logs",
  label: "服务日志",
  description: "读取某个服务最近的输出日志，用于定位启动失败或运行时错误。",
  promptSnippet: "读取某个服务最近的日志（默认 150 行）",
  promptGuidelines: ["排查服务异常时先用 pc_service_logs 看真实报错，再下结论，不要凭日志之外的猜测归因。"],
  parameters: Type.Object(
    {
      service: Type.String({ description: "服务名（pc_project_context 里列出的 name）" }),
      lines: Type.Optional(
        Type.Integer({ minimum: 1, maximum: 400, description: "读取的行数，1–400，省略时为 150" }),
      ),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(
      ctx,
      "logs",
      { service: params.service, lines: params.lines ?? 150 },
      { timeout: 30_000, signal },
    );
    return { content: [{ type: "text", text: jsonText(result) }], details: result };
  },
});

const proposeServicesTool = defineTool({
  name: "pc_propose_services",
  label: "提交服务配方",
  description:
    "把完整的服务配方集合交给 app，由用户确认后写盘。提交的是覆盖集合：不在集合里的现有服务会被删除。",
  promptSnippet: "提交完整的服务配方集合（覆盖语义，省略的现有服务会被删除）",
  promptGuidelines: [
    "pc_propose_services 是完整集合覆盖语义：每次调用都必须提交全部服务，只提交改动的那一个会删掉其余服务。",
    "修正某个服务的配置时，把其余服务原样一起提交。",
    "提交前自查：argv 是数组、路径都是项目内相对路径、ports.listen 不为 0、depends_on 只填服务名。",
  ],
  parameters: Type.Object(
    {
      services: Type.Array(ServiceSpecSchema, {
        minItems: 1,
        description: "完整的服务集合（覆盖现有配置）",
      }),
      notes: Type.String({ description: "给用户看的改动说明：为什么这样配、需要注意什么" }),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "propose_services", params, {
      timeout: 7_200_000,
      signal,
    });
    return { content: [{ type: "text", text: jsonText(result) }], details: result };
  },
});

const reportDiagnosisTool = defineTool({
  name: "pc_report_diagnosis",
  label: "提交诊断",
  description:
    "提交一个服务的问题诊断：现象、根因、归类、证据与修复建议。属于配置问题时必须同时给出完整的 config_fix。",
  promptSnippet: "提交某个服务的诊断结论（现象/根因/证据/建议，配置问题附带完整 config_fix）",
  promptGuidelines: [
    "诊断必须以证据为准：evidence 里放真实文件路径、行号与原文片段，不要空口断言。",
    "category 选 config 时，必须同时给出 config_fix，且 config_fix 是完整的服务集合（覆盖语义）。",
  ],
  parameters: Type.Object(
    {
      service: Type.String({ description: "出问题的服务名" }),
      summary: Type.String({ description: "一句话结论：这个服务怎么了" }),
      cause: Type.String({ description: "根因分析：为什么会这样" }),
      category: Type.Union(
        [
          Type.Literal("config"),
          Type.Literal("code"),
          Type.Literal("env"),
          Type.Literal("external"),
          Type.Literal("unknown"),
        ],
        { description: "config=配方配错，code=代码问题，env=环境/依赖缺失，external=外部依赖，unknown=说不清" },
      ),
      evidence: Type.Array(
        Type.Object(
          {
            file: Type.Optional(Type.String({ description: "项目内相对路径" })),
            line: Type.Optional(Type.Integer({ minimum: 1, description: "行号" })),
            excerpt: Type.String({ description: "原文片段（短，够定位即可）" }),
          },
          { additionalProperties: false },
        ),
        { description: "证据列表" },
      ),
      suggestion: Type.String({ description: "建议的修复动作（人可以照着做的那种）" }),
      config_fix: Type.Optional(
        Type.Array(ServiceSpecSchema, {
          minItems: 1,
          description: "category 为 config 时的完整服务集合（覆盖语义）",
        }),
      ),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "diagnosis", params, { timeout: 30_000, signal });
    return { content: [{ type: "text", text: jsonText(result) }], details: result, terminate: true };
  },
});

const reportHealthTool = defineTool({
  name: "pc_report_health",
  label: "提交健康检查",
  description: "提交全项目健康检查结论：一组按严重程度排序的问题条目，可带可执行的排查命令。",
  promptSnippet: "提交健康检查结论（error/warn/info 条目，可带排查命令）",
  promptGuidelines: [
    "pc_report_health 一轮只调一次，把所有发现一次交齐，不要逐条反复调用。",
    "items 按严重程度排序：error 在最前；没有问题时提交一条 info 说明检查过什么。",
  ],
  parameters: Type.Object(
    {
      summary: Type.String({ description: "整体结论，一两句话" }),
      items: Type.Array(
        Type.Object(
          {
            severity: Type.Union([Type.Literal("error"), Type.Literal("warn"), Type.Literal("info")], {
              description: "严重程度",
            }),
            title: Type.String({ description: "问题标题，短句" }),
            detail: Type.String({ description: "细节：现象、影响面、相关文件" }),
            command: Type.Optional(Type.String({ description: "用户可自行执行的只读排查命令，如 pnpm -v" })),
          },
          { additionalProperties: false },
        ),
        { description: "问题条目" },
      ),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "health", params, { timeout: 30_000, signal });
    return { content: [{ type: "text", text: jsonText(result) }], details: result, terminate: true };
  },
});

const proposeProfileTool = defineTool({
  name: "pc_propose_profile",
  label: "提交项目简介",
  description: "提交项目简介：一句话说明、技术栈与标签。用于项目卡片展示。",
  promptSnippet: "提交项目简介（一句话说明 + 技术栈 + 标签）",
  promptGuidelines: ["profile 只能从实际读到的内容里总结，不要编造技术栈。"],
  parameters: Type.Object(
    {
      summary: Type.String({ maxLength: 120, description: "一句话说明这个项目做什么（≤120 字）" }),
      stack: Type.Array(Type.String(), { maxItems: 12, description: "技术栈，最多 12 项" }),
      tags: Type.Array(Type.String(), { maxItems: 6, description: "标签，最多 6 个" }),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "profile", params, { timeout: 30_000, signal });
    return { content: [{ type: "text", text: jsonText(result) }], details: result, terminate: true };
  },
});

const proposeReadmeTool = defineTool({
  name: "pc_propose_readme",
  label: "提交 README",
  description: "提交一份为这个项目新写的 README 全文（Markdown），由用户确认后写盘。",
  promptSnippet: "提交完整的 README 正文（Markdown，≤60000 字）",
  promptGuidelines: [
    "README 面向第一次接触项目的人：写清怎么装、怎么跑、有哪些服务与端口。",
    "命令块必须与项目的真实脚本一致，不要写不存在的 npm script。",
  ],
  parameters: Type.Object(
    {
      content: Type.String({ maxLength: 60_000, description: "README 全文（Markdown）" }),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "readme", params, { timeout: 30_000, signal });
    return { content: [{ type: "text", text: jsonText(result) }], details: result, terminate: true };
  },
});

const serviceControlTool = defineTool({
  name: "pc_service_control",
  label: "启停服务",
  description: "请求 app 启动、停止或重启某个服务。需要用户确认后才会真正执行。",
  promptSnippet: "请求启停/重启某个服务（需用户确认）",
  promptGuidelines: [
    "pc_service_control 只有用户明确要求启停时才调用；reason 写清为什么要动这个服务。",
    "调用返回后不要假设服务已就绪，用 pc_service_logs 或 pc_report_diagnosis 跟结果。",
  ],
  parameters: Type.Object(
    {
      service: Type.String({ description: "服务名" }),
      action: Type.Union([Type.Literal("start"), Type.Literal("stop"), Type.Literal("restart")], {
        description: "start / stop / restart",
      }),
      reason: Type.String({ description: "为什么要执行这个动作" }),
    },
    { additionalProperties: false },
  ),

  async execute(_toolCallId, params, signal, _onUpdate, ctx) {
    const result = await hostCall<unknown>(ctx, "service_control", params, { timeout: 900_000, signal });
    return { content: [{ type: "text", text: jsonText(result) }], details: result };
  },
});

/* ------------------------------------------------------------------------- *
 * 扩展入口。
 * ------------------------------------------------------------------------- */

export default function (pi: ExtensionAPI) {
  // 闸门：所有工具调用（含 pc_* 自己）都先问 app。拿不到明确答案一律拦截——
  // 宁可让模型换个办法，也不要在没有授权的情况下动本地文件。
  pi.on("tool_call", async (event, ctx) => {
    try {
      const raw = await ctx.ui.input(
        GATE_TITLE,
        JSON.stringify({
          v: 1,
          toolCallId: event.toolCallId,
          tool: event.toolName,
          input: event.input,
        }),
        { timeout: GATE_TIMEOUT_MS },
      );
      if (raw === undefined) {
        return { block: true, reason: "权限检查超时，已拦截" };
      }

      const decision = asRecord(JSON.parse(raw)) as GateReply | undefined;
      if (decision?.allow === false) {
        const reason = decision.reason;
        return { block: true, reason: typeof reason === "string" ? reason : "已拦截" };
      }

      const patch = asRecord(decision?.patch);
      if (patch !== undefined) {
        Object.assign(event.input, patch);
      }
      return undefined;
    } catch {
      return { block: true, reason: "权限检查失败，已拦截" };
    }
  });

  pi.registerTool(projectContextTool);
  pi.registerTool(serviceLogsTool);
  pi.registerTool(proposeServicesTool);
  pi.registerTool(reportDiagnosisTool);
  pi.registerTool(reportHealthTool);
  pi.registerTool(proposeProfileTool);
  pi.registerTool(proposeReadmeTool);
  pi.registerTool(serviceControlTool);
}
