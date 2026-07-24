import * as net from "node:net";
import { randomUUID } from "node:crypto";

const PROTOCOL_VERSION = 1;
const MAX_RESPONSE_BYTES = 1024 * 1024;

interface ToolResponse {
	version: number;
	request_id: string;
	content: unknown;
	details: unknown;
	is_error: boolean;
	error_code?: string;
	error_details?: unknown;
}

export async function callAgentHost(
	toolName: string,
	argumentsValue: Record<string, unknown>,
	signal?: AbortSignal,
): Promise<ToolResponse> {
	const endpoint = process.env.PI_WHIM_AGENT_HOST;
	const capability = process.env.PI_WHIM_AGENT_CAPABILITY;
	if (!endpoint || !capability) throw new Error("Pi-Whim agent supervisor is unavailable");
	const separator = endpoint.lastIndexOf(":");
	const host = endpoint.slice(0, separator);
	const port = Number(endpoint.slice(separator + 1));
	if (!host || !Number.isInteger(port)) throw new Error("Invalid Pi-Whim agent supervisor endpoint");

	const requestId = randomUUID();
	const request = JSON.stringify({
		version: PROTOCOL_VERSION,
		request_id: requestId,
		capability,
		tool_name: toolName,
		arguments: argumentsValue,
	});

	return await new Promise<ToolResponse>((resolve, reject) => {
		const socket = net.createConnection({ host, port });
		let settled = false;
		let buffer = "";
		const finish = (error?: Error, response?: ToolResponse) => {
			if (settled) return;
			settled = true;
			signal?.removeEventListener("abort", abort);
			socket.destroy();
			if (error) reject(error);
			else if (response) resolve(response);
		};
		const abort = () => finish(new Error("Agent tool call aborted"));
		if (signal?.aborted) return abort();
		signal?.addEventListener("abort", abort, { once: true });
		// The Rust host enforces command-specific limits. Keep the transport timeout
		// above the largest supported foreground timeout so long jobs are not cut off
		// by the thin TypeScript bridge.
		socket.setTimeout(86_405_000, () => finish(new Error("Agent supervisor timed out")));
		socket.on("connect", () => socket.write(`${request}\n`));
		socket.on("data", (chunk) => {
			buffer += chunk.toString("utf8");
			if (Buffer.byteLength(buffer, "utf8") > MAX_RESPONSE_BYTES) {
				finish(new Error("Agent supervisor response exceeded 1 MiB"));
				return;
			}
			const newline = buffer.indexOf("\n");
			if (newline < 0) return;
			try {
				const response = JSON.parse(buffer.slice(0, newline)) as ToolResponse;
				if (response.request_id !== requestId) throw new Error("Agent supervisor response ID mismatch");
				finish(undefined, response);
			} catch (error) {
				finish(error instanceof Error ? error : new Error(String(error)));
			}
		});
		socket.on("error", (error) => finish(error));
		socket.on("end", () => {
			if (!settled) finish(new Error("Agent supervisor closed without a response"));
		});
	});
}

export function responseText(response: ToolResponse): string {
	if (response.is_error) {
		const content = response.content as { message?: string } | undefined;
		const error = new Error(`${response.error_code ?? "agent_error"}: ${content?.message ?? "Agent tool failed"}`);
		(error as Error & { details?: unknown }).details = response.error_details;
		throw error;
	}
	return JSON.stringify(response.content, null, 2);
}
