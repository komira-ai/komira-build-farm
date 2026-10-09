# `step`: one command, run as `/bin/sh -c <script> step <out> <srcs...>`, so the script
# reads its output path as "$1" and its inputs as "$2" onwards.

def _step_impl(ctx):
    out = ctx.actions.declare_output(ctx.label.name + ".txt")
    ctx.actions.run(
        cmd_args("/bin/sh", "-c", ctx.attrs.script, "step", out.as_output(), ctx.attrs.srcs),
        category = "step",
    )
    return [DefaultInfo(default_output = out)]

step = rule(
    impl = _step_impl,
    attrs = {
        "script": attrs.string(),
        "srcs": attrs.list(attrs.source()),
    },
)

# `pause`: one command that sleeps `secs` seconds, then writes `salt` to its output. Its
# argv holds both, so a new salt is a new action: never an action-cache hit, it runs
# on the farm and holds its lease for `secs` seconds.

def _pause_impl(ctx):
    out = ctx.actions.declare_output(ctx.label.name + ".txt")
    ctx.actions.run(
        cmd_args(
            "/bin/sh",
            "-c",
            'sleep "$2" && printf "%s\\n" "$3" > "$1"',
            "pause",
            out.as_output(),
            ctx.attrs.secs,
            ctx.attrs.salt,
        ),
        category = "pause",
    )
    return [DefaultInfo(default_output = out)]

pause = rule(
    impl = _pause_impl,
    attrs = {
        "salt": attrs.string(),
        "secs": attrs.string(),
    },
)
