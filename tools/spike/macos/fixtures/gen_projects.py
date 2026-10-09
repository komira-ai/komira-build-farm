#!/usr/bin/env python3
"""Writes the two XCUITest fixture projects (MacFixture.xcodeproj, IOSFixture.xcodeproj).

Each project has an app target and a UI test target, and one shared scheme whose test
action runs the UI tests. The files are generated, not edited by hand, so a reviewer
reads this script instead of the pbxproj; `--check` regenerates in memory and fails if
a committed file differs, so the two cannot drift.

    python3 gen_projects.py           # write the projects next to this script
    python3 gen_projects.py --check   # exit 1 if a committed file differs

Object identifiers are derived from names (md5), so the output is the same on every run.
Nothing is signed with an identity: macOS builds sign to run locally (ad-hoc, "-").
The iOS project lists both iOS platforms (SUPPORTED_PLATFORMS), but only a simulator
build runs without a team; run.sh tests it on a simulator destination only.
"""

import hashlib
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# Swift sources, relative to this directory (the projects' SOURCE_ROOT).
APP_SOURCES = ["App/FixtureApp.swift", "App/ContentView.swift", "Common/Pattern.swift"]
TEST_SOURCES = [
    "UITests/FixtureUITests.swift",
    "UITests/PatternCheck.swift",
    "Common/Pattern.swift",
]

PLATFORMS = {
    "MacFixture": {
        "bundle": "com.example.kbf.fixture.mac",
        "settings": {
            "SDKROOT": "macosx",
            "MACOSX_DEPLOYMENT_TARGET": "13.0",
            "SUPPORTED_PLATFORMS": "macosx",
        },
        "app": {
            "COMBINE_HIDPI_IMAGES": "YES",
            "ENABLE_HARDENED_RUNTIME": "NO",
            "LD_RUNPATH_SEARCH_PATHS": '"@executable_path/../Frameworks"',
        },
        "tests": {
            "LD_RUNPATH_SEARCH_PATHS": '"@loader_path/../Frameworks"',
        },
    },
    "IOSFixture": {
        "bundle": "com.example.kbf.fixture.ios",
        "settings": {
            "SDKROOT": "iphoneos",
            "IPHONEOS_DEPLOYMENT_TARGET": "16.0",
            "SUPPORTED_PLATFORMS": '"iphonesimulator iphoneos"',
            "TARGETED_DEVICE_FAMILY": "1",
        },
        "app": {
            "INFOPLIST_KEY_UIApplicationSceneManifest_Generation": "YES",
            "INFOPLIST_KEY_UILaunchScreen_Generation": "YES",
            "LD_RUNPATH_SEARCH_PATHS": '"@executable_path/Frameworks"',
        },
        "tests": {
            "LD_RUNPATH_SEARCH_PATHS": '"@loader_path/Frameworks"',
        },
    },
}

# Settings shared by every target of both projects. CODE_SIGN_IDENTITY "-" is
# "Sign to Run Locally": an ad-hoc signature, no certificate or team.
COMMON = {
    "ALWAYS_SEARCH_USER_PATHS": "NO",
    "CLANG_ENABLE_MODULES": "YES",
    "CODE_SIGN_IDENTITY": '"-"',
    "CODE_SIGN_STYLE": "Manual",
    "CURRENT_PROJECT_VERSION": "1",
    "DEVELOPMENT_TEAM": '""',
    "GENERATE_INFOPLIST_FILE": "YES",
    "MARKETING_VERSION": "1.0",
    "PRODUCT_NAME": '"$(TARGET_NAME)"',
    "SWIFT_VERSION": "5.0",
}
DEBUG = {
    "DEBUG_INFORMATION_FORMAT": "dwarf",
    "ENABLE_TESTABILITY": "YES",
    "ONLY_ACTIVE_ARCH": "YES",
    "SWIFT_OPTIMIZATION_LEVEL": '"-Onone"',
}
RELEASE = {
    "DEBUG_INFORMATION_FORMAT": '"dwarf-with-dsym"',
    "SWIFT_OPTIMIZATION_LEVEL": '"-O"',
}


def oid(*parts):
    """A stable 24-hex-digit object identifier for a name."""
    return hashlib.md5("/".join(parts).encode()).hexdigest()[:24].upper()


def settings_block(d, indent):
    pad = "\t" * indent
    return "".join(f"{pad}{k} = {v};\n" for k, v in sorted(d.items()))


def config(ident, name, settings):
    return (
        f"\t\t{ident} /* {name} */ = {{\n"
        "\t\t\tisa = XCBuildConfiguration;\n"
        "\t\t\tbuildSettings = {\n"
        f"{settings_block(settings, 4)}"
        "\t\t\t};\n"
        f"\t\t\tname = {name};\n"
        "\t\t};\n"
    )


def config_list(ident, owner, debug_id, release_id):
    return (
        f"\t\t{ident} /* Build configuration list for {owner} */ = {{\n"
        "\t\t\tisa = XCConfigurationList;\n"
        "\t\t\tbuildConfigurations = (\n"
        f"\t\t\t\t{debug_id} /* Debug */,\n"
        f"\t\t\t\t{release_id} /* Release */,\n"
        "\t\t\t);\n"
        "\t\t\tdefaultConfigurationIsVisible = 0;\n"
        "\t\t\tdefaultConfigurationName = Debug;\n"
        "\t\t};\n"
    )


def pbxproj(name):
    p = PLATFORMS[name]
    tests = f"{name}UITests"
    app_product, test_product = f"{name}.app", f"{tests}.xctest"
    ids = {
        "project": oid(name, "project"),
        "main_group": oid(name, "group", "main"),
        "products_group": oid(name, "group", "products"),
        "app_product": oid(name, "product", "app"),
        "tests_product": oid(name, "product", "tests"),
        "app": oid(name, "target", "app"),
        "tests": oid(name, "target", "tests"),
        "proxy": oid(name, "proxy"),
        "dependency": oid(name, "dependency"),
    }
    sources = sorted(set(APP_SOURCES + TEST_SOURCES))
    file_ref = {s: oid(name, "file", s) for s in sources}
    build_file = {
        (t, s): oid(name, "build", t, s)
        for t, srcs in (("app", APP_SOURCES), ("tests", TEST_SOURCES))
        for s in srcs
    }

    out = ["// !$*UTF8*$!\n{\n\tarchiveVersion = 1;\n\tclasses = {\n\t};\n"]
    out.append("\tobjectVersion = 56;\n\tobjects = {\n")

    out.append("\n/* Begin PBXBuildFile section */\n")
    for (t, s), ident in sorted(build_file.items(), key=lambda kv: kv[1]):
        base = os.path.basename(s)
        out.append(
            f"\t\t{ident} /* {base} in Sources */ = {{isa = PBXBuildFile; "
            f"fileRef = {file_ref[s]} /* {base} */; }};\n"
        )
    out.append("/* End PBXBuildFile section */\n")

    out.append("\n/* Begin PBXContainerItemProxy section */\n")
    out.append(
        f"\t\t{ids['proxy']} /* PBXContainerItemProxy */ = {{\n"
        "\t\t\tisa = PBXContainerItemProxy;\n"
        f"\t\t\tcontainerPortal = {ids['project']} /* Project object */;\n"
        "\t\t\tproxyType = 1;\n"
        f"\t\t\tremoteGlobalIDString = {ids['app']};\n"
        f"\t\t\tremoteInfo = {name};\n"
        "\t\t};\n"
    )
    out.append("/* End PBXContainerItemProxy section */\n")

    out.append("\n/* Begin PBXFileReference section */\n")
    refs = [
        (
            ids["app_product"],
            f"{{isa = PBXFileReference; explicitFileType = wrapper.application; "
            f"includeInIndex = 0; path = {app_product}; sourceTree = BUILT_PRODUCTS_DIR; }}",
            app_product,
        ),
        (
            ids["tests_product"],
            f"{{isa = PBXFileReference; explicitFileType = wrapper.cfbundle; "
            f"includeInIndex = 0; path = {test_product}; sourceTree = BUILT_PRODUCTS_DIR; }}",
            test_product,
        ),
    ]
    for s in sources:
        refs.append(
            (
                file_ref[s],
                f"{{isa = PBXFileReference; lastKnownFileType = sourcecode.swift; "
                f"path = {s}; sourceTree = SOURCE_ROOT; }}",
                os.path.basename(s),
            )
        )
    for ident, body, comment in sorted(refs):
        out.append(f"\t\t{ident} /* {comment} */ = {body};\n")
    out.append("/* End PBXFileReference section */\n")

    out.append("\n/* Begin PBXFrameworksBuildPhase section */\n")
    for t in ("app", "tests"):
        out.append(
            f"\t\t{oid(name, 'frameworks', t)} /* Frameworks */ = {{\n"
            "\t\t\tisa = PBXFrameworksBuildPhase;\n"
            "\t\t\tbuildActionMask = 2147483647;\n"
            "\t\t\tfiles = (\n\t\t\t);\n"
            "\t\t\trunOnlyForDeploymentPostprocessing = 0;\n"
            "\t\t};\n"
        )
    out.append("/* End PBXFrameworksBuildPhase section */\n")

    out.append("\n/* Begin PBXGroup section */\n")
    children = "".join(
        f"\t\t\t\t{file_ref[s]} /* {os.path.basename(s)} */,\n" for s in sources
    )
    out.append(
        f"\t\t{ids['main_group']} = {{\n"
        "\t\t\tisa = PBXGroup;\n"
        "\t\t\tchildren = (\n"
        f"{children}"
        f"\t\t\t\t{ids['products_group']} /* Products */,\n"
        "\t\t\t);\n"
        '\t\t\tsourceTree = "<group>";\n'
        "\t\t};\n"
        f"\t\t{ids['products_group']} /* Products */ = {{\n"
        "\t\t\tisa = PBXGroup;\n"
        "\t\t\tchildren = (\n"
        f"\t\t\t\t{ids['app_product']} /* {app_product} */,\n"
        f"\t\t\t\t{ids['tests_product']} /* {test_product} */,\n"
        "\t\t\t);\n"
        "\t\t\tname = Products;\n"
        '\t\t\tsourceTree = "<group>";\n'
        "\t\t};\n"
    )
    out.append("/* End PBXGroup section */\n")

    out.append("\n/* Begin PBXNativeTarget section */\n")
    for t, tname, product, ptype, deps in (
        ("app", name, app_product, "com.apple.product-type.application", ""),
        (
            "tests",
            tests,
            test_product,
            "com.apple.product-type.bundle.ui-testing",
            f"\t\t\t\t{ids['dependency']} /* PBXTargetDependency */,\n",
        ),
    ):
        out.append(
            f"\t\t{ids[t]} /* {tname} */ = {{\n"
            "\t\t\tisa = PBXNativeTarget;\n"
            f"\t\t\tbuildConfigurationList = {oid(name, 'configlist', t)} "
            f"/* Build configuration list for PBXNativeTarget \"{tname}\" */;\n"
            "\t\t\tbuildPhases = (\n"
            f"\t\t\t\t{oid(name, 'sources', t)} /* Sources */,\n"
            f"\t\t\t\t{oid(name, 'frameworks', t)} /* Frameworks */,\n"
            f"\t\t\t\t{oid(name, 'resources', t)} /* Resources */,\n"
            "\t\t\t);\n"
            "\t\t\tbuildRules = (\n\t\t\t);\n"
            f"\t\t\tdependencies = (\n{deps}\t\t\t);\n"
            f"\t\t\tname = {tname};\n"
            f"\t\t\tproductName = {tname};\n"
            f"\t\t\tproductReference = {ids[t + '_product']} /* {product} */;\n"
            f'\t\t\tproductType = "{ptype}";\n'
            "\t\t};\n"
        )
    out.append("/* End PBXNativeTarget section */\n")

    out.append("\n/* Begin PBXProject section */\n")
    out.append(
        f"\t\t{ids['project']} /* Project object */ = {{\n"
        "\t\t\tisa = PBXProject;\n"
        "\t\t\tattributes = {\n"
        "\t\t\t\tBuildIndependentTargetsInParallel = 1;\n"
        "\t\t\t\tLastSwiftUpdateCheck = 1500;\n"
        "\t\t\t\tLastUpgradeCheck = 1500;\n"
        "\t\t\t\tTargetAttributes = {\n"
        f"\t\t\t\t\t{ids['tests']} = {{\n"
        f"\t\t\t\t\t\tTestTargetID = {ids['app']};\n"
        "\t\t\t\t\t};\n"
        "\t\t\t\t};\n"
        "\t\t\t};\n"
        f"\t\t\tbuildConfigurationList = {oid(name, 'configlist', 'project')} "
        f'/* Build configuration list for PBXProject "{name}" */;\n'
        '\t\t\tcompatibilityVersion = "Xcode 14.0";\n'
        "\t\t\tdevelopmentRegion = en;\n"
        "\t\t\thasScannedForEncodings = 0;\n"
        "\t\t\tknownRegions = (\n\t\t\t\ten,\n\t\t\t\tBase,\n\t\t\t);\n"
        f"\t\t\tmainGroup = {ids['main_group']};\n"
        f"\t\t\tproductRefGroup = {ids['products_group']} /* Products */;\n"
        '\t\t\tprojectDirPath = "";\n'
        '\t\t\tprojectRoot = "";\n'
        "\t\t\ttargets = (\n"
        f"\t\t\t\t{ids['app']} /* {name} */,\n"
        f"\t\t\t\t{ids['tests']} /* {tests} */,\n"
        "\t\t\t);\n"
        "\t\t};\n"
    )
    out.append("/* End PBXProject section */\n")

    out.append("\n/* Begin PBXResourcesBuildPhase section */\n")
    for t in ("app", "tests"):
        out.append(
            f"\t\t{oid(name, 'resources', t)} /* Resources */ = {{\n"
            "\t\t\tisa = PBXResourcesBuildPhase;\n"
            "\t\t\tbuildActionMask = 2147483647;\n"
            "\t\t\tfiles = (\n\t\t\t);\n"
            "\t\t\trunOnlyForDeploymentPostprocessing = 0;\n"
            "\t\t};\n"
        )
    out.append("/* End PBXResourcesBuildPhase section */\n")

    out.append("\n/* Begin PBXSourcesBuildPhase section */\n")
    for t, srcs in (("app", APP_SOURCES), ("tests", TEST_SOURCES)):
        files = "".join(
            f"\t\t\t\t{build_file[(t, s)]} /* {os.path.basename(s)} in Sources */,\n"
            for s in srcs
        )
        out.append(
            f"\t\t{oid(name, 'sources', t)} /* Sources */ = {{\n"
            "\t\t\tisa = PBXSourcesBuildPhase;\n"
            "\t\t\tbuildActionMask = 2147483647;\n"
            f"\t\t\tfiles = (\n{files}\t\t\t);\n"
            "\t\t\trunOnlyForDeploymentPostprocessing = 0;\n"
            "\t\t};\n"
        )
    out.append("/* End PBXSourcesBuildPhase section */\n")

    out.append("\n/* Begin PBXTargetDependency section */\n")
    out.append(
        f"\t\t{ids['dependency']} /* PBXTargetDependency */ = {{\n"
        "\t\t\tisa = PBXTargetDependency;\n"
        f"\t\t\ttarget = {ids['app']} /* {name} */;\n"
        f"\t\t\ttargetProxy = {ids['proxy']} /* PBXContainerItemProxy */;\n"
        "\t\t};\n"
    )
    out.append("/* End PBXTargetDependency section */\n")

    out.append("\n/* Begin XCBuildConfiguration section */\n")
    target_settings = {
        "project": dict(p["settings"], **COMMON),
        "app": dict(p["app"], PRODUCT_BUNDLE_IDENTIFIER=p["bundle"]),
        "tests": dict(
            p["tests"],
            PRODUCT_BUNDLE_IDENTIFIER=p["bundle"] + ".uitests",
            TEST_TARGET_NAME=name,
        ),
    }
    configs = []
    for owner, base in target_settings.items():
        for cname, extra in (("Debug", DEBUG if owner == "project" else {}),
                             ("Release", RELEASE if owner == "project" else {})):
            configs.append((oid(name, "config", owner, cname), cname, dict(base, **extra)))
    for ident, cname, s in sorted(configs):
        out.append(config(ident, cname, s))
    out.append("/* End XCBuildConfiguration section */\n")

    out.append("\n/* Begin XCConfigurationList section */\n")
    lists = []
    for owner, label in (
        ("project", f'PBXProject "{name}"'),
        ("app", f'PBXNativeTarget "{name}"'),
        ("tests", f'PBXNativeTarget "{tests}"'),
    ):
        lists.append(
            (
                oid(name, "configlist", owner),
                label,
                oid(name, "config", owner, "Debug"),
                oid(name, "config", owner, "Release"),
            )
        )
    for ident, label, d, r in sorted(lists):
        out.append(config_list(ident, label, d, r))
    out.append("/* End XCConfigurationList section */\n")

    out.append(f"\t}};\n\trootObject = {ids['project']} /* Project object */;\n}}\n")
    return "".join(out), ids


def scheme(name, ids):
    tests = f"{name}UITests"
    container = f"container:{name}.xcodeproj"

    def ref(ident, product, tname):
        return (
            "            <BuildableReference\n"
            '               BuildableIdentifier = "primary"\n'
            f'               BlueprintIdentifier = "{ident}"\n'
            f'               BuildableName = "{product}"\n'
            f'               BlueprintName = "{tname}"\n'
            f'               ReferencedContainer = "{container}">\n'
            "            </BuildableReference>\n"
        )

    app_ref = ref(ids["app"], f"{name}.app", name)
    test_ref = ref(ids["tests"], f"{tests}.xctest", tests)
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<Scheme\n   LastUpgradeVersion = "1500"\n   version = "1.7">\n'
        '   <BuildAction\n      parallelizeBuildables = "YES"\n'
        '      buildImplicitDependencies = "YES">\n'
        "      <BuildActionEntries>\n"
        "         <BuildActionEntry\n"
        '            buildForTesting = "YES"\n            buildForRunning = "YES"\n'
        '            buildForProfiling = "YES"\n            buildForArchiving = "YES"\n'
        '            buildForAnalyzing = "YES">\n'
        f"{app_ref}"
        "         </BuildActionEntry>\n"
        "      </BuildActionEntries>\n"
        "   </BuildAction>\n"
        "   <TestAction\n"
        '      buildConfiguration = "Debug"\n'
        '      selectedDebuggerIdentifier = "Xcode.DebuggerFoundation.Debugger.LLDB"\n'
        '      selectedLauncherIdentifier = "Xcode.DebuggerFoundation.Launcher.LLDB"\n'
        '      shouldUseLaunchSchemeArgsEnv = "YES">\n'
        "      <Testables>\n"
        "         <TestableReference\n"
        '            skipped = "NO">\n'
        f"{test_ref}"
        "         </TestableReference>\n"
        "      </Testables>\n"
        "   </TestAction>\n"
        "   <LaunchAction\n"
        '      buildConfiguration = "Debug"\n'
        '      selectedDebuggerIdentifier = "Xcode.DebuggerFoundation.Debugger.LLDB"\n'
        '      selectedLauncherIdentifier = "Xcode.DebuggerFoundation.Launcher.LLDB"\n'
        '      launchStyle = "0"\n'
        '      useCustomWorkingDirectory = "NO"\n'
        '      ignoresPersistentStateOnLaunch = "NO"\n'
        '      debugDocumentVersioning = "YES"\n'
        '      debugServiceExtension = "internal"\n'
        '      allowLocationSimulation = "YES">\n'
        "      <BuildableProductRunnable\n"
        '         runnableDebuggingMode = "0">\n'
        f"{app_ref.replace('            ', '         ')}"
        "      </BuildableProductRunnable>\n"
        "   </LaunchAction>\n"
        "</Scheme>\n"
    )


def outputs():
    """Every generated file: {path relative to this directory: text}."""
    files = {}
    for name in sorted(PLATFORMS):
        proj, ids = pbxproj(name)
        files[f"{name}.xcodeproj/project.pbxproj"] = proj
        files[f"{name}.xcodeproj/xcshareddata/xcschemes/{name}.xcscheme"] = scheme(name, ids)
    return files


def main(argv):
    check = argv[1:] == ["--check"]
    if argv[1:] and not check:
        print(__doc__, file=sys.stderr)
        return 2
    stale = []
    for rel, text in outputs().items():
        path = os.path.join(HERE, rel)
        if check:
            try:
                with open(path, encoding="utf-8") as f:
                    if f.read() != text:
                        stale.append(rel)
            except FileNotFoundError:
                stale.append(rel)
        else:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)
    if stale:
        print("generated files differ from gen_projects.py (run it and commit):", file=sys.stderr)
        for rel in stale:
            print(f"  {rel}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
