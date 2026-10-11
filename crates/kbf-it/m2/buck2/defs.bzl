# `act`: one run of `tools/kbf-m2-act` (the static helper run.sh copies into the
# project; the image has no shell), as `kbf-m2-act <verb> <out> <args...> <srcs...>`.
# The helper is an input file of every action, so each one that runs proves the farm
# kept its executable bit.

def _act_impl(ctx):
    out = ctx.actions.declare_output(ctx.label.name + ".txt")
    ctx.actions.run(
        cmd_args(ctx.attrs.tool, ctx.attrs.verb, out.as_output(), ctx.attrs.args, ctx.attrs.srcs),
        category = ctx.attrs.verb,
    )
    return [DefaultInfo(default_output = out)]

_act = rule(
    impl = _act_impl,
    attrs = {
        "args": attrs.list(attrs.string(), default = []),
        "srcs": attrs.list(attrs.source(), default = []),
        "tool": attrs.source(),
        "verb": attrs.string(),
    },
)

def act(**kwargs):
    _act(tool = "tools/kbf-m2-act", **kwargs)
