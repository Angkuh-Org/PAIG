# SSE and response shape

- `"stream": true` returns `text/event-stream` with chunked transfer,
  `cache-control: no-cache, no-transform`, and `x-accel-buffering: no`.
- Every backend streams OpenAI `chat.completion.chunk` frames; Command Code's
  CLI stream is translated, OpenAI-shaped backends pass through as-is.
- Non-streaming requests return one `chat.completion` object with the `model`
  field restored to the id the caller sent.
- Errors use `{"error":{"message":...,"type":...,"code":...}}`; upstream
  failures keep their HTTP status where possible. A mid-stream failure emits
  an OpenAI error frame and closes the stream.

Passthrough backends preserve unknown request fields.
