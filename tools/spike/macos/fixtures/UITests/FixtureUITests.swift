// The fixture's UI tests, one source for macOS and iOS. Each test prints one
// `KBF-FIXTURE <test>=PASS|FAIL <detail>` line, which run.sh copies out of the
// xcodebuild log, and every screenshot is kept in the .xcresult as an attachment
// (lifetime keepAlways), whatever the outcome.

import XCTest

final class FixtureUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    /// Proves the button and label work through the accessibility layer: a tap reaches
    /// the app and the test reads the new label back. Turns red if the tap is lost or
    /// the app sets another label.
    func testTapChangesLabel() {
        let app = launched()
        let label = app.staticTexts["label"]
        XCTAssertEqual(text(of: label), "ready")
        app.buttons["button"].tap()
        let after = waitForText(of: label, "tapped 1")
        report("tap", after == "tapped 1", "label after one tap: \(after)")
        XCTAssertEqual(after, "tapped 1")
    }

    /// Proves the screenshot shows what the app painted: the window screenshot must
    /// hold the pattern. The whole-screen screenshot is recorded, not asserted,
    /// because what it shows depends on the session's Screen Recording grant (the
    /// question the probes ask).
    func testScreenshotShowsPattern() {
        let app = launched()
        let window = app.windows.firstMatch.screenshot()
        keep(window, "window")
        let verdict = PatternCheck.check(png: window.pngRepresentation)
        let screen = XCUIScreen.main.screenshot()
        keep(screen, "screen")
        let onScreen = PatternCheck.check(png: screen.pngRepresentation)
        print("KBF-FIXTURE screen-pattern=\(onScreen.found ? "found" : "missing") "
              + onScreen.reason)
        report("window-pattern", verdict.found, verdict.reason)
        XCTAssertTrue(verdict.found, verdict.reason)
    }

    /// The control arm: with the pattern left out, the checker must find none. Turns
    /// red if the checker passes any screenshot (so the test above could not fail).
    func testPatternLeftOutIsNotFound() {
        let app = launched(arguments: [FixturePattern.hideArgument])
        XCTAssertFalse(app.otherElements["pattern"].exists)
        let window = app.windows.firstMatch.screenshot()
        keep(window, "window-no-pattern")
        let verdict = PatternCheck.check(png: window.pngRepresentation)
        report("control-no-pattern", !verdict.found, verdict.reason)
        XCTAssertFalse(verdict.found, "found a pattern the app did not paint: \(verdict.reason)")
    }

    private func launched(arguments: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = arguments
        app.launch()
        XCTAssertTrue(app.staticTexts["label"].waitForExistence(timeout: 60),
                      "the app's label never appeared")
        return app
    }

    /// A static text's string: `label` on iOS; on macOS SwiftUI puts it in `value`.
    private func text(of element: XCUIElement) -> String {
        if let v = element.value as? String, !v.isEmpty { return v }
        return element.label
    }

    private func waitForText(of element: XCUIElement, _ want: String) -> String {
        let deadline = Date().addingTimeInterval(10)
        var got = text(of: element)
        while got != want && Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
            got = text(of: element)
        }
        return got
    }

    private func keep(_ shot: XCUIScreenshot, _ name: String) {
        let attachment = XCTAttachment(screenshot: shot)
        attachment.name = name
        // MUTANT V1: lifetime left at its default
        add(attachment)
    }

    private func report(_ test: String, _ pass: Bool, _ detail: String) {
        print("KBF-FIXTURE \(test)=\(pass ? "PASS" : "FAIL") \(detail)")
    }
}
