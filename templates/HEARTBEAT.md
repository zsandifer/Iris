# Heartbeat

Checklist for scheduled check-ins. When check-ins are on (Settings, "Check
in on its own", or `[heartbeat]` in config.toml), the agent reads this file on a schedule and
messages you in Main chat only when something needs attention.

## Checks

1. **Reminders**: anything in memory that is due or overdue?
2. **Follow-ups**: any "I'll get back to you" items older than a day?
3. **Goals**: any goal whose status should be updated?
4. **Connectors**: anything urgent in connected services (email, calendar)?

## Rules

- If nothing needs attention, reply with exactly `HEARTBEAT_OK`.
- Batch several items into one short message.
