# LLM providers

clashtui is distributed as a single process/binary, so the built-in provider
catalog is compiled into the binary from `doc/llm-providers.yaml`.

At runtime clashtui also maintains a local provider catalog:

- `llm-providers.yaml` in the clashtui config directory
- created from the bundled catalog if missing
- contains provider endpoints, model ids, and the user's API keys
- local API keys are stored in provider `api_key`
- custom model ids are stored in provider `models`
- `config.yaml` stores the selected provider, base URL, and model, but not the
  API key itself

Provider updates are manual. The Runtime page action `Update LLM Providers`
merges the current bundled catalog into the local file:

- built-in provider fields are refreshed from the bundled catalog
- local `api_key` values are preserved
- local custom model ids are preserved and appended
- local custom providers not present in the bundled catalog are preserved

Do not assume a normal pay-as-you-go endpoint and a coding/token plan endpoint
are interchangeable. Some China-region providers use distinct endpoints and
keys for coding plans:

- Kimi Platform: `https://api.moonshot.cn/v1`
- Kimi Code Plan: `https://api.kimi.com/coding/v1`
- Qwen DashScope CN: `https://dashscope.aliyuncs.com/compatible-mode/v1`
- Qwen Coding Plan: `https://coding.dashscope.aliyuncs.com/v1`
- Volcengine Ark: `https://ark.cn-beijing.volces.com/api/v3`
- Ark Coding Plan: `https://ark.cn-beijing.volces.com/api/coding/v3`
- Baidu Qianfan: `https://qianfan.baidubce.com/v2`
- Qianfan Coding Plan: `https://qianfan.baidubce.com/v2/coding`

When a user reports authentication, quota, or model-not-found errors, verify the
selected provider, base URL, model id, and API key source together. A valid API
key for a normal platform may fail on the coding/token endpoint, and the reverse
can also be true.
