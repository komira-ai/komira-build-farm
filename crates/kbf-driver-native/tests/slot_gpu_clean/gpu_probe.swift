// The GPU work of tests/slot_gpu.rs: a Metal device, a shader compiled at run time and a
// compute kernel run to completion. One `STAGE` line per step, so that a failure names
// the step; `GPU_PROBE_PASS` is the last line when all of them passed.
import Foundation
import Metal

func stage(_ name: String, _ ok: Bool, _ detail: String = "") {
    print("STAGE \(name) \(ok ? "ok" : "FAIL") \(detail)")
    fflush(stdout)
}

guard let device = MTLCreateSystemDefaultDevice() else {
    stage("device", false, "no default device")
    exit(3)
}
stage("device", true, device.name)

let source = """
#include <metal_stdlib>
using namespace metal;
kernel void twice(device const float* a [[buffer(0)]], device float* b [[buffer(1)]],
                  uint i [[thread_position_in_grid]]) {
    b[i] = a[i] * 2.0;
}
"""
guard let queue = device.makeCommandQueue() else { stage("queue", false); exit(4) }
let library: MTLLibrary
do { library = try device.makeLibrary(source: source, options: nil) } catch {
    stage("shader-compile", false, "\(error)")
    exit(5)
}
stage("shader-compile", true)
guard let function = library.makeFunction(name: "twice"),
      let pipeline = try? device.makeComputePipelineState(function: function) else {
    stage("pipeline", false)
    exit(6)
}
stage("pipeline", true)

let count = 4096
let input = (0..<count).map { Float($0) }
guard let a = device.makeBuffer(bytes: input, length: count * 4, options: .storageModeShared),
      let b = device.makeBuffer(length: count * 4, options: .storageModeShared),
      let commands = queue.makeCommandBuffer(),
      let encoder = commands.makeComputeCommandEncoder() else {
    stage("encode", false)
    exit(7)
}
encoder.setComputePipelineState(pipeline)
encoder.setBuffer(a, offset: 0, index: 0)
encoder.setBuffer(b, offset: 0, index: 1)
encoder.dispatchThreads(MTLSize(width: count, height: 1, depth: 1),
                        threadsPerThreadgroup: MTLSize(width: 64, height: 1, depth: 1))
encoder.endEncoding()
commands.commit()
commands.waitUntilCompleted()
let output = b.contents().bindMemory(to: Float.self, capacity: count)
let wrong = (0..<count).filter { output[$0] != input[$0] * 2 }.count
stage("kernel", commands.status == .completed && wrong == 0, "wrong=\(wrong)")
guard commands.status == .completed && wrong == 0 else { exit(8) }
print("GPU_PROBE_PASS")
fflush(stdout)
// With `GPU_HOLD` set, keep the device, queue and buffers open until the process is ended.
if ProcessInfo.processInfo.environment["GPU_HOLD"] != nil {
    print("HOLD_READY")
    fflush(stdout)
    withExtendedLifetime((device, queue, a, b, pipeline)) { sleep(600) }
}
exit(0)
