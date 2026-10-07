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
