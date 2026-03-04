# Code generation

`esk generate` produces typed config files from your secret definitions. Validation constraints (format, enum, pattern, range, length) carry through to the generated output.

## Formats

| Format        | Description                                                                                                                                |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `dts`         | TypeScript declarations that augment `ProcessEnv`. Enum secrets become union types; optional secrets get `?`.                              |
| `ts`          | Runtime module that validates and parses all env vars eagerly at import time. Only the helpers actually used are emitted.                  |
| `ts-lazy`     | Same as `ts`, but each property is a getter — validation happens on first access instead of at import time.                                |
| `zod`         | Zod schema that validates `process.env` at import time. Constraints map directly to Zod chainables (`.min()`, `.max()`, `.regex()`, etc.). |
| `env-example` | `.env.example` template with descriptions as comments, enum allowed values listed, and optional secrets commented out.                     |

## Usage

Preview output without writing files:

```
esk generate ts --preview
```

Write to a specific file:

```
esk generate dts -o env.d.ts
```

### Config-driven generation

Define outputs in `esk.yaml` to run all at once with `esk generate`:

```yaml
generate:
  - format: dts
  - format: ts
    output: src/env.ts
  - format: zod
    output: src/env-zod.ts
  - format: env-example
```

When no `output` is specified, esk uses a default path based on the format.

## Key behaviors

- **Tree-shaking**: `ts` and `ts-lazy` only emit the runtime helpers that are actually referenced (e.g., `requiredBool` only appears if a boolean secret exists).
- **Optional handling**: Optional typed secrets (bool, int, json) use `optional*` helpers that return `undefined` instead of throwing. Optional strings with no constraints use bare `process.env.KEY` — no helper needed.
- **Enum types**: `dts` emits union literal types; `ts`/`ts-lazy` pass `{ allowed: [...] }` to the helper; `zod` uses `z.enum()`.
- **Regex patterns**: `ts`, `ts-lazy`, and `zod` use `new RegExp("...")` instead of `/.../` to avoid breaking on patterns containing `/`.
- **Coercion**: `zod` uses `z.coerce.number()` for numeric types since env vars are always strings.
