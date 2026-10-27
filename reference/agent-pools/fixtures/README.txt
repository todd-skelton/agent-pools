Provider payloads recorded 2026-09-24 through the pool's own read path
(GET only; nothing was redeemed), then scrubbed: account, user, email, credit
and grant ids, labels, profile fields and plan tier are replaced with fixture
values. Structure, enums, numbers and timestamps are as the providers sent them.

  codex-usage-available.json  GET chatgpt.com/backend-api/wham/usage (allowed)
  codex-usage-exhausted.json  same endpoint, an account at its weekly limit
  codex-credits.json          GET .../wham/rate-limit-reset-credits with one credit
  claude-usage.json           GET api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1
                              as Claude Code (cedar_ember is undocumented)

refresh.test.mjs runs the refresh cycle against these. If a provider changes
its format, re-record with the same scrubbing and expect those tests to show
what moved.
