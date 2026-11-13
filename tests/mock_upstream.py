#!/usr/bin/env python3
"""Mock upstream used by tests/integration.sh. No third-party deps."""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CC_SSE = "".join([
    'data: {"type":"text-delta","text":"hello"}\n\n',
    'data: {"type":"text-delta","text":" world"}\n\n',
    'data: {"type":"finish","rawFinishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":2}}\n\n',
    "data: [DONE]\n\n",
]).encode()

CC_NO_DONE = "".join([
    'data: {"type":"text-delta","text":"hello"}\n\n',
    'data: {"type":"text-delta","text":" world"}\n\n',
    'data: {"type":"finish","rawFinishReason":"stop","totalUsage":{"inputTokens":5,"outputTokens":2}}\n\n',
]).encode()

OPENAI_SSE = "".join([
    'data: {"id":"mock","object":"chat.completion.chunk","created":1,"model":"mock","choices":[{"index":0,"delta":{"role":"assistant","content":"open"},"finish_reason":null}]}\n\n',
    'data: {"id":"mock","object":"chat.completion.chunk","created":1,"model":"mock","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n',
    'data: {"id":"mock","object":"chat.completion.chunk","created":1,"model":"mock","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}\n\n',
    "data: [DONE]\n\n",
]).encode()

OPENAI_JSON = json.dumps({
    "id": "mock",
    "object": "chat.completion",
    "created": 1,
    "model": "mock",
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "open"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
}).encode()

MODELS = json.dumps({
    "object": "list",
    "data": [
        {
            "id": "test-model",
            "object": "model",
            "created": 1,
            "owned_by": "mock",
            "context_length": 1000000,
        }
    ],
}).encode()

MISTRAL_MODELS = json.dumps({
    "object": "list",
    "data": [
        {
            "id": "mistral-model",
            "object": "model",
            "created": 1,
            "owned_by": "mistralai",
            "max_context_length": 256000,
            "capabilities": {
                "completion_chat": True,
                "function_calling": True,
                "vision": True,
                "reasoning": False,
            },
        }
    ],
}).encode()

MODELS_DEV = json.dumps({
    "commandcode": {
        "models": {
            "test-model": {
                "limit": {"context": 999, "output": 128000},
                "tool_call": True,
                "reasoning": True,
                "vision": None,
                "modalities": {"input": ["text", "image"]},
                "cost": {"input": 3, "output": 15},
            }
        }
    },
    "opencode": {
        "models": {
            "beam-free": {
                "limit": {"context": 1048576, "output": 131072},
                "tool_call": True,
                "reasoning": True,
                "modalities": {"input": ["text"]},
                "cost": {"input": 0, "output": 0},
            }
        }
    },
    "opencode-go": {
        "models": {
            "beam-free": {
                "limit": {"context": 1048576, "output": 131072},
                "tool_call": True,
                "reasoning": True,
                "modalities": {"input": ["text"]},
                "cost": {"input": 0, "output": 0},
            }
        }
    },
    "mistral": {
        "models": {
            "mistral-model": {
                "limit": {"context": 999, "output": 64000},
                "tool_call": True,
                "reasoning": False,
                "cost": {"input": 0, "output": 0},
            }
        }
    },
}).encode()

BOB_MODELS = json.dumps({
    "object": "list",
    "data": [
        {
            "model_name": "bob-model",
            "model_info": {
                "max_input_tokens": 131072,
                "max_output_tokens": 65536,
                "supports_vision": True,
                "supports_prompt_caching": False,
                "input_cost_per_token": 0,
                "output_cost_per_token": 0.000002,
            },
        }
    ],
}).encode()

BOB_PROFILE = json.dumps({
    "instances": [{"teams": [{"id": "team1"}]}],
}).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    def send_body(self, status, content_type, body):
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        if self.path == "/alpha/generate":
            try:
                body = json.loads(raw or b"{}")
            except Exception:
                body = {}
            model = body.get("params", {}).get("model", "")
            if model == "empty-model":
                self.send_response(204)
                self.send_header("content-length", "0")
                self.end_headers()
                return
            if model == "not-modified-model":
                self.send_response(304)
                self.end_headers()
                return
            payload = CC_NO_DONE if model == "no-done-model" else CC_SSE
            self.send_body(200, "text/event-stream", payload)
            return
        if self.path.endswith("/chat/completions"):
            try:
                body = json.loads(raw or b"{}")
            except Exception:
                body = {}
            if body.get("stream") is True:
                self.send_body(200, "text/event-stream", OPENAI_SSE)
            else:
                self.send_body(200, "application/json", OPENAI_JSON)
            return
        self.send_body(404, "application/json", b'{"error":{"message":"not found"}}')

    def do_GET(self):
        if self.path == "/models-dev.json":
            self.send_body(200, "application/json", MODELS_DEV)
            return
        if self.path.endswith("/model/info"):
            self.send_body(200, "application/json", BOB_MODELS)
            return
        if self.path.endswith("/admin/v1/profile"):
            self.send_body(200, "application/json", BOB_PROFILE)
            return
        if self.path.endswith("/provider/v1/models"):
            self.send_body(200, "application/json", MODELS)
            return
        if self.path.endswith("/models"):
            if self.headers.get("authorization"):
                self.send_body(200, "application/json", MISTRAL_MODELS)
            else:
                self.send_body(200, "application/json", MODELS)
            return
        self.send_body(404, "application/json", b'{"error":{"message":"not found"}}')


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 19090
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
