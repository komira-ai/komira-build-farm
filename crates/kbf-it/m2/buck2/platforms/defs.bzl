# The only execution platform: remote, with local execution switched off, so every
# command runs on the farm or is answered from its action cache. Its properties are
# `OSFamily=linux` and, when `.buckconfig` names one, `container-image`: left out, the
# actions still reach the farm, and the container driver refuses each one.

def _remote_platform_impl(ctx):
    properties = {"OSFamily": "linux"}
    if ctx.attrs.container_image:
        properties["container-image"] = ctx.attrs.container_image
    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = ConfigurationInfo(constraints = {}, values = {}),
        executor_config = CommandExecutorConfig(
            local_enabled = False,
            remote_enabled = True,
            use_limited_hybrid = False,
            remote_execution_properties = properties,
            remote_execution_use_case = "buck2-default",
            remote_output_paths = "output_paths",
        ),
    )
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]

remote_platform = rule(
    impl = _remote_platform_impl,
    attrs = {"container_image": attrs.string()},
)
