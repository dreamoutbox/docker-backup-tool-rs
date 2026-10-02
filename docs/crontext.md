# `crontext` Schedule Reference

`dvb` supports natural-language schedule expressions via the `crontext` field as an alternative to raw 5-field `cron` expressions.

At configuration load time, `crontext` expressions are parsed and resolved into standard 5-field cron strings for the scheduler daemon.

## Configuration

Each job must define **exactly one** of `cron` or `crontext`. Setting both or neither is a configuration validation error.

```toml
[[job]]
name = "postgres"
crontext = "every friday at 18:00"
timezone = "America/New_York"       # optional per-job timezone override
# ...
```

## CLI Inspection

You can test schedule expressions and inspect upcoming fire times directly from the CLI without needing a configuration file:

```bash
dvb crontext "every friday at 18:00"
dvb crontext "every 15 minutes" --timezone Europe/London
```

This outputs the resolved 5-field cron, description, effective timezone, and next 5 fire times.

## Supported Expressions

```crontext
every minute
every 15 minutes
every hour
every 12 hours
every day
every 1 day
every day at 03:30
every 12:00
every monday
every friday at 18:00
every mon, wed and fri at 06:30
every weekday at 09:00
every weekend at 10:00
every month
every month on the 1st at 03:00
```

## Rules & Constraints

- **Interval Steps:** Minute intervals (`every N minutes`) must divide 60 cleanly (e.g. 1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30). Hour intervals (`every N hours`) must divide 24 cleanly (e.g. 1, 2, 3, 4, 6, 8, 12).
- **Day of Month:** Monthly schedules (`every month on the Nth`) only allow days 1 through 28 to avoid skipped schedules on short months (such as February).
- **Time Format:** 24-hour format (`HH:MM` or `H:MM`, `00:00` to `23:59`) or 12-hour format (`12am`, `6pm`, `6:30pm`).
