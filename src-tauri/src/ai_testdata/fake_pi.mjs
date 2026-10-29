#!/usr/bin/env node
/**
 * `pi --mode rpc` 的测试替身：只实现 ai_session 集成测试用到的协议子集。
 *
 * 支持的命令：`get_state` / `get_available_models` / `prompt` / `abort` /
 * `set_model` / `set_thinking_level`。其余命令一律回 `success:false`（让测试暴露漏实现）。
 *
 * prompt 里的关键字：
 * - `TOOL`  → 先走一次 `pc-gate` 权限请求（bash + `rm -rf /`，真实闸门会拒绝），
 *             再发 `tool_execution_start/end`；
 * - `HOST`  → 发一次 `pc-host` 扩展 UI 请求，把宿主回包原文拼进下一条助手消息；
 * - `PROVIDER_ERROR` → 非可重试的模型调用失败：`message_start`（空内容）+
 *             `message_end{stopReason:"error", errorMessage:"400: boom"}` + `agent_settled`；
 *             `PROVIDER_ERROR_AUTH` 变体把 errorMessage 换成认证类文本（应触发 onboarding 卡）；
 * - `RETRY_ERROR` → 重试耗尽（真实 pi 的顺序：每轮 `message_end{error}` 先到、
 *             再 `auto_retry_start`）：3 次重试 + `auto_retry_end{success:false}`，
 *             结算时只能出现一条终态错误提示；
 * - `RETRY_OK` → 先失败再重试成功：不能出现任何错误提示，只留一条「重试成功」；
 * - `MIXED_ERROR` → 同一轮两处上报且文本不同（`400: boom` vs `Retry exhausted`），
 *             仍然只能出现一条错误提示；
 * - `PROVIDER_ERROR_THEN_CRASH` → 先报 `message_end{error}` 再以退出码 3 退出：
 *             结算时先出模型失败、再出崩溃/恢复提示；
 * - `PROVIDER_ERROR_NO_START` → 失败回合一上来就是 `message_end`（没有 `message_start`）；
 * - `PROVIDER_ERROR_NO_SETTLE` → 报错后不再 settle（会话停在 busy，供 abort 用例使用）；
 * - `CRASH` → 回完 prompt 应答后以退出码 3 退出（触发崩溃恢复）；
 * - `HANG`  → 流式吐到一半就再也不 settle（触发看门狗）。
 *
 * 其余 prompt：`message_start` → 若干 `text_delta`（拼出「你好世界」）→ `message_end`
 * → `agent_settled`。
 *
 * 开关既可以走环境变量（`FAKE_PI_MODE` / `FAKE_PI_ARGV_LOG`，供手工调试），
 * 也可以走前置参数（`--fake-mode` / `--fake-argv-log`）——Rust 测试用参数形式，
 * 避免改进程级环境变量与并行测试互相干扰。
 */

import { appendFileSync } from "node:fs";

const argv = process.argv.slice(2);

/** 取 `--flag value` 形式的前置参数。 */
function flag(name) {
	const i = argv.indexOf(name);
	return i >= 0 ? argv[i + 1] : undefined;
}

/** `--session-id x` 的值（pi 的真实参数，用来验证恢复时复用了同一个会话）。 */
function sessionIdArg() {
	return flag("--session-id");
}

const MODE = flag("--fake-mode") ?? process.env.FAKE_PI_MODE ?? "";
const ARGV_LOG = flag("--fake-argv-log") ?? process.env.FAKE_PI_ARGV_LOG;

// 每次 spawn 追加一行 argv：测试据此数「起了几次进程」以及「session-id 是否复用」。
if (ARGV_LOG) {
	try {
		appendFileSync(ARGV_LOG, `${JSON.stringify(argv)}\n`);
	} catch {
		// 日志写不进去不能影响协议行为。
	}
}

const state = {
	model: { provider: "fake", id: "fake-model", name: "Fake Model" },
	thinkingLevel: flag("--thinking") ?? "medium",
	isStreaming: false,
};

let seq = 0;
const pendingUi = new Map();

/** 写一条协议记录（严格一行一个 JSON）。 */
function send(obj) {
	process.stdout.write(`${JSON.stringify(obj)}\n`);
}

/** 命令应答。 */
function respond(id, command, success, data, error) {
	const rec = { type: "response", command, success };
	if (id !== undefined && id !== null) rec.id = id;
	if (success) rec.data = data ?? null;
	else rec.error = error ?? "failed";
	send(rec);
}

/** 发一条扩展 UI 请求并等宿主回包。 */
function askUi(method, title, placeholder) {
	const id = `ui-${++seq}`;
	return new Promise((resolve) => {
		pendingUi.set(id, resolve);
		send({ type: "extension_ui_request", id, method, title, placeholder });
	});
}

/** 把文本按小段切开，模拟真实的流式增量。 */
function emitAssistant(text) {
	send({ type: "message_start", message: { role: "assistant", content: [] } });
	const chars = [...text];
	const size = Math.max(1, Math.ceil(chars.length / 4));
	for (let i = 0; i < chars.length; i += size) {
		const delta = chars.slice(i, i + size).join("");
		send({
			type: "message_update",
			usage: { input: 1, output: 1, totalTokens: 2 },
			assistantMessageEvent: { type: "text_delta", contentIndex: 0, delta },
		});
	}
	send({
		type: "message_end",
		message: { role: "assistant", content: [{ type: "text", text }], stopReason: "end" },
	});
}

/** 一次完整的助手回合收尾。 */
function settle(text) {
	emitAssistant(text);
	send({ type: "agent_settled", aborted: false });
}

/** `TOOL`：先过闸门，再发一对工具执行事件。 */
async function runGatedTool() {
	const toolCallId = "call_gate";
	const args = { command: "rm -rf /" };
	const raw = await askUi(
		"input",
		"pc-gate",
		JSON.stringify({ v: 1, toolCallId, tool: "bash", input: args }),
	);
	let allow = false;
	try {
		allow = JSON.parse(raw ?? "{}").allow === true;
	} catch {
		allow = false;
	}
	send({ type: "tool_execution_start", toolCallId, toolName: "bash", args });
	send({
		type: "tool_execution_end",
		toolCallId,
		toolName: "bash",
		result: { content: [{ type: "text", text: allow ? "executed" : "已拦截" }], details: {} },
		isError: !allow,
		durationMs: 1,
	});
}

/** 处理一条 prompt。 */
async function onPrompt(id, message) {
	const text = typeof message === "string" ? message : "";
	if (!text.includes("HANG")) {
		respond(id, "prompt", true, { disposition: "started" });
	}

	if (text.includes("PROVIDER_ERROR_THEN_CRASH")) {
		// 报了一条模型失败就崩：结算时先报失败原因，再报崩溃/恢复。
		// 必须排在 `CRASH` 之前——否则会被那句宽泛的子串匹配先吃掉。
		send({ type: "message_start", message: { role: "assistant", content: [] } });
		send({
			type: "message_end",
			message: { role: "assistant", content: [], stopReason: "error", errorMessage: "400: then crash" },
		});
		// 事件必须真的落到管道里再退：否则客户端看不到这条失败。
		process.stdout.write("", () => process.exit(3));
		return;
	}

	if (text.includes("CRASH")) {
		// 应答必须真的落到管道里再退出，否则测试看到的是空应答。
		process.stdout.write("", () => process.exit(3));
		return;
	}

	state.isStreaming = true;
	if (MODE === "garbage") {
		// 夹一条非 JSON 行：客户端必须容忍并继续解析后续记录。
		process.stdout.write("这不是 JSON，客户端应当忽略\n");
	}

	let echo = "";
	if (text.includes("TOOL")) {
		await runGatedTool();
	}
	if (text.includes("HOST")) {
		const raw = await askUi("input", "pc-host", JSON.stringify({ v: 1, method: "probe", params: { x: 1 } }));
		let reply = "null";
		try {
			// 宿主回包是 `{"ok":true,"result":…}`；把 result 原样带进正文，供测试断言。
			reply = JSON.stringify(JSON.parse(raw ?? "null").result);
		} catch {
			reply = String(raw);
		}
		echo = `宿主回声 ${reply}\n`;
	}

	if (text.includes("HANG")) {
		// 吐一半就不再 settle：看门狗 / abort 应当收掉这个进程。
		emitAssistant("半");
		return;
	}

	if (text.includes("RETRY_OK")) {
		// 首次失败后重试成功：最终不能有任何错误提示，只留一条「重试成功」。
		send({
			type: "message_end",
			message: { role: "assistant", content: [], stopReason: "error", errorMessage: "Connection error." },
		});
		send({ type: "auto_retry_start", attempt: 1, maxAttempts: 1, errorMessage: "Connection error." });
		emitAssistant("好了");
		send({ type: "auto_retry_end", success: true });
		send({ type: "agent_settled", aborted: false });
		return;
	}

	if (text.includes("RETRY_ERROR")) {
		// 真实 pi 的顺序：每次 `message_end{error}` 都先于 `auto_retry_start` 到达，
		// 每次重试都是一条新的空 assistant 消息；耗尽后给 finalError，再 settle。
		// 用户最后看到的必须是「重试失败」，不是「正在重试（3/3）」。
		const attemptFailed = () => {
			send({ type: "message_start", message: { role: "assistant", content: [] } });
			send({
				type: "message_end",
				message: {
					role: "assistant",
					content: [],
					stopReason: "error",
					errorMessage: "Connection error.",
				},
			});
		};
		attemptFailed();
		for (let attempt = 1; attempt <= 3; attempt++) {
			send({
				type: "auto_retry_start",
				attempt,
				maxAttempts: 3,
				errorMessage: "Connection error.",
			});
			attemptFailed();
		}
		send({ type: "auto_retry_end", success: false, finalError: "Connection error." });
		send({ type: "agent_settled", aborted: false });
		return;
	}

	if (text.includes("MIXED_ERROR")) {
		// 同一轮里两处失败上报：message_end 的 errorMessage 与 auto_retry_end 的
		// finalError 文本不同，但只能给用户一条结论。
		send({ type: "message_start", message: { role: "assistant", content: [] } });
		send({
			type: "message_end",
			message: { role: "assistant", content: [], stopReason: "error", errorMessage: "400: boom" },
		});
		send({ type: "auto_retry_start", attempt: 1, maxAttempts: 1, errorMessage: "400: boom" });
		send({ type: "auto_retry_end", success: false, finalError: "Retry exhausted" });
		send({ type: "agent_settled", aborted: false });
		return;
	}

	if (text.includes("PROVIDER_ERROR_NO_SETTLE")) {
		// 有错误但永不 settle：会话卡在 busy，供 abort 路径清标记的用例使用。
		send({ type: "message_start", message: { role: "assistant", content: [] } });
		send({
			type: "message_end",
			message: { role: "assistant", content: [], stopReason: "error", errorMessage: "400: stalled" },
		});
		return;
	}

	if (text.includes("PROVIDER_ERROR_NO_START")) {
		// 最坏情况：失败回合连 `message_start` 都没有，只有一条 message_end。
		send({
			type: "message_end",
			message: {
				role: "assistant",
				content: [],
				stopReason: "error",
				errorMessage: "400: no start",
			},
		});
		send({ type: "agent_settled", aborted: false });
		return;
	}

	if (text.includes("PROVIDER_ERROR")) {
		// 非可重试失败（400 / 认证）：只有一条 content 为空的 error message_end。
		const errorMessage = text.includes("PROVIDER_ERROR_AUTH")
			? "No API key found for the selected model."
			: "400: boom";
		send({ type: "message_start", message: { role: "assistant", content: [] } });
		send({
			type: "message_end",
			message: {
				role: "assistant",
				content: [],
				stopReason: "error",
				errorMessage,
			},
		});
		send({ type: "agent_settled", aborted: false });
		return;
	}

	state.isStreaming = false;
	settle(`${echo}你好世界`);
}

/** 逐行处理 stdin（只按 \n 切，与 pi 的分帧规则一致）。 */
let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
	buf += chunk;
	let idx = buf.indexOf("\n");
	while (idx >= 0) {
		let line = buf.slice(0, idx);
		buf = buf.slice(idx + 1);
		if (line.endsWith("\r")) line = line.slice(0, -1);
		if (line.trim() !== "") handleLine(line);
		idx = buf.indexOf("\n");
	}
});
process.stdin.on("end", () => process.exit(0));

function handleLine(line) {
	let rec;
	try {
		rec = JSON.parse(line);
	} catch {
		return;
	}
	const id = typeof rec.id === "string" ? rec.id : undefined;

	if (rec.type === "extension_ui_response") {
		const resolve = pendingUi.get(rec.id);
		if (resolve) {
			pendingUi.delete(rec.id);
			resolve(rec.cancelled === true ? undefined : rec.value);
		}
		return;
	}

	switch (rec.type) {
		case "get_state":
			if (MODE === "hang_start") return; // 不回应答：握手超时路径
			respond(id, "get_state", true, {
				model: state.model,
				thinkingLevel: state.thinkingLevel,
				isStreaming: state.isStreaming,
				isCompacting: false,
				sessionFile: `/tmp/fake-${sessionIdArg() ?? "none"}.jsonl`,
				sessionId: sessionIdArg() ?? "fake",
				messageCount: 0,
			});
			break;
		case "get_available_models":
			respond(id, "get_available_models", true, {
				models: [
					{
						provider: "fake",
						id: "fake-model",
						name: "Fake Model",
						reasoning: false,
						contextWindow: 1000,
					},
				],
			});
			break;
		case "set_model":
			state.model = {
				provider: rec.provider ?? "fake",
				id: rec.modelId ?? "fake-model",
				name: rec.modelId ?? "fake-model",
			};
			respond(id, "set_model", true, state.model);
			break;
		case "set_thinking_level":
			state.thinkingLevel = rec.level ?? "medium";
			respond(id, "set_thinking_level", true, { level: state.thinkingLevel });
			break;
		case "abort":
			state.isStreaming = false;
			respond(id, "abort", true, null);
			// abort 的语义是「回到空闲」，真实 pi 会补一条 agent_settled。
			process.nextTick(() => send({ type: "agent_settled", aborted: true }));
			break;
		case "prompt":
			void onPrompt(id, rec.message);
			break;
		case "steer":
		case "follow_up":
			respond(id, rec.type, true, { disposition: "queued" });
			break;
		default:
			respond(id, rec.type ?? "unknown", false, null, `unsupported command: ${rec.type}`);
	}
}

// 启动时往 stderr 留一行：握手失败的通知要能带上 stderr 内容。
process.stderr.write("[fake-pi] 已启动\n");
