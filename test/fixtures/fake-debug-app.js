'use strict';
// A stand-in for an app's DebugTraceServer, for exercising the Debug view
// without a device. It speaks the same envelope ({ok, endpoint, elapsedMs,
// data} / {ok:false, error:{code, message, hint}}) on `GET /`, `POST /_call`
// and `GET|POST /<endpoint>`, and its `_profile` / `_memory` replies are shaped
// like the real ones (DebugTrace's DebugProfiler.swift and DebugMemory.swift),
// with numbers taken from an ExampleApp run on a Vision Pro.
//
// Options:
//   token        bearer token it insists on (null: open)
//   approvalMs   the first request from each client name waits this long, as
//                if the person at the device were reading the prompt
//   approval     'allow' (default) or 'timeout': answer the wait with the 403
//                DebugTrace sends when nobody tapped Allow
const http = require('node:http');

const MB = 1024 * 1024;

const ENDPOINTS = [
  { name: '_info', kind: 'query', description: 'Identity and health: app, build SHA, device, process, trace settings.' },
  { name: '_logs', kind: 'query', description: "The app's own log lines from the in-process buffer.",
    parameters: [
      { name: 'level', type: 'string', required: false, default: 'info', choices: ['debug', 'info', 'notice', 'error', 'fault'], description: 'Lowest level returned.' },
      { name: 'contains', type: 'string', required: false, description: 'Only lines containing this text.' },
      { name: 'limit', type: 'integer', required: false, default: 200, min: 1, max: 5000, description: 'Most lines returned.' },
    ] },
  { name: '_features', kind: 'query', description: 'Feature breadcrumbs: what was used, in order.' },
  { name: '_profile', kind: 'query',
    description: 'Samples thread stacks for a few seconds and returns where the time went: per thread, the busiest functions by self and inclusive samples, and the hottest whole stacks.',
    parameters: [
      { name: 'seconds', type: 'number', required: false, default: 2, min: 0.2, max: 10, description: 'How long to sample.' },
      { name: 'intervalMs', type: 'number', required: false, default: 2, min: 0.5, max: 50, description: 'Time between samples.' },
      { name: 'threads', type: 'string', required: false, default: 'main', choices: ['main', 'all'], description: 'The main thread only, or every thread.' },
      { name: 'limit', type: 'integer', required: false, default: 12, min: 1, max: 50, description: 'Rows per table and stacks per thread.' },
    ] },
  { name: '_memory', kind: 'query',
    description: 'Footprint, jetsam headroom, malloc zones, Metal allocations and the largest VM region kinds.',
    parameters: [
      { name: 'regions', type: 'integer', required: false, default: 12, min: 1, max: 60, description: 'How many region kinds to list, largest first.' },
    ] },
  { name: '_snapshot', kind: 'query', description: 'Everything a debug trace captures, as JSON.' },
  { name: 'renderStats', kind: 'query', description: 'Frame pacing and draw counts from the voxel renderer.' },
  { name: '_trace', kind: 'command', description: 'Builds a debug trace zip and optionally uploads it.',
    parameters: [
      { name: 'note', type: 'string', required: false, description: 'What you are looking into.' },
      { name: 'upload', type: 'boolean', required: false, default: false, description: 'Upload to the app store server.' },
    ] },
  { name: 'setWeather', kind: 'command', destructive: true, description: 'Changes the world weather.',
    parameters: [{ name: 'kind', type: 'string', required: true, choices: ['clear', 'rain', 'snow'], description: 'Weather to switch to.' }] },
];

const err = (status, code, message, hint) => ({ status, body: { ok: false, error: { code, message, hint } } });

// ---- _profile ----
const row = (fn, image, samples, total) => ({ function: fn, image, samples, percent: Math.round(samples / total * 1000) / 10 });
const stack = (samples, total, frames) => ({ samples, percent: Math.round(samples / total * 1000) / 10, frames });

const RUNLOOP = ['start', 'UIApplicationMain', '-[UIApplication _run]', 'GSEventRunModal', 'CFRunLoopRunSpecific', '__CFRunLoopRun'];
const TICK = [...RUNLOOP, '__CFRunLoopDoObservers', 'ExampleApp.GameLoop.step(timestamp: Swift.Double) -> ()'];

function mainThread(n, limit) {
  const t = n;
  const stacks = [
    stack(Math.round(t * .112), t, [...TICK, 'ExampleApp.PortalTransit.tick(deltaTime: Swift.Double) -> ()', 'ExampleApp.World.emissiveSurfaces(near: Swift.SIMD3<Swift.Float>, radius: Swift.Float) -> Swift.Array<ExampleApp.EmissiveSurface>']),
    stack(Math.round(t * .071), t, [...TICK, 'ExampleApp.PortalTransit.tick(deltaTime: Swift.Double) -> ()', 'ExampleApp.PortalTransit.updateStencil(for: ExampleApp.Portal) -> ()', 'simd_inverse']),
    stack(Math.round(t * .064), t, [...TICK, 'ExampleApp.ChunkStreamer.update(playerPosition: Swift.SIMD3<Swift.Float>) -> ()', 'Swift.Dictionary.subscript.getter : (A) -> Swift.Optional<B>', 'swift_retain']),
    stack(Math.round(t * .041), t, [...TICK, 'ExampleApp.ChunkStreamer.update(playerPosition: Swift.SIMD3<Swift.Float>) -> ()', 'MacroTerrain.HeightField.sample(x: Swift.Int, z: Swift.Int) -> Swift.Float']),
    stack(Math.round(t * .033), t, [...RUNLOOP, '__CFRunLoopDoSources0', '-[UIApplication sendEvent:]', 'RAVEInput.HandTracker.consume(anchors: Swift.Array<ARKit.HandAnchor>) -> ()', 'RAVEInput.PinchDetector.evaluate(_: ARKit.HandSkeleton) -> RAVEInput.PinchState']),
    stack(Math.round(t * .022), t, [...RUNLOOP, '__CFRunLoopDoTimers', 'ExampleApp.AutosaveController.flush() -> ()', 'Foundation.JSONEncoder.encode<A where A: Swift.Encodable>(A) throws -> Foundation.Data', 'swift_allocObject', 'malloc_zone_malloc', '_xzm_xzone_malloc']),
    stack(Math.round(t * .018), t, [...RUNLOOP, '__CFRunLoopDoObservers', 'CA::Transaction::commit()', 'CA::Context::commit_transaction(CA::Transaction*, double, double*)', 'CA::Layer::layout_and_display_if_needed(CA::Transaction*)']),
    stack(Math.round(t * .012), t, [...RUNLOOP, '__CFRunLoopDoSources0', 'objc_msgSend']),
  ].slice(0, limit);
  const self = [
    row('ExampleApp.World.emissiveSurfaces(near: Swift.SIMD3<Swift.Float>, radius: Swift.Float) -> Swift.Array<ExampleApp.EmissiveSurface>', 'ExampleApp', Math.round(t * .112), t),
    row('simd_inverse', 'libsystem_m.dylib', Math.round(t * .071), t),
    row('swift_retain', 'libswiftCore.dylib', Math.round(t * .069), t),
    row('MacroTerrain.HeightField.sample(x: Swift.Int, z: Swift.Int) -> Swift.Float', 'ExampleApp', Math.round(t * .041), t),
    row('RAVEInput.PinchDetector.evaluate(_: ARKit.HandSkeleton) -> RAVEInput.PinchState', 'ExampleApp', Math.round(t * .033), t),
    row('_xzm_xzone_malloc', 'libsystem_malloc.dylib', Math.round(t * .024), t),
    row('CA::Layer::layout_and_display_if_needed(CA::Transaction*)', 'QuartzCore', Math.round(t * .018), t),
    row('objc_msgSend', 'libobjc.A.dylib', Math.round(t * .014), t),
    row('swift_release', 'libswiftCore.dylib', Math.round(t * .009), t),
  ].slice(0, limit);
  const incl = [
    row('ExampleApp.GameLoop.step(timestamp: Swift.Double) -> ()', 'ExampleApp', Math.round(t * .288), t),
    row('ExampleApp.PortalTransit.tick(deltaTime: Swift.Double) -> ()', 'ExampleApp', Math.round(t * .183), t),
    row('ExampleApp.World.emissiveSurfaces(near: Swift.SIMD3<Swift.Float>, radius: Swift.Float) -> Swift.Array<ExampleApp.EmissiveSurface>', 'ExampleApp', Math.round(t * .112), t),
    row('ExampleApp.ChunkStreamer.update(playerPosition: Swift.SIMD3<Swift.Float>) -> ()', 'ExampleApp', Math.round(t * .105), t),
    row('ExampleApp.PortalTransit.updateStencil(for: ExampleApp.Portal) -> ()', 'ExampleApp', Math.round(t * .071), t),
    row('-[UIApplication sendEvent:]', 'UIKitCore', Math.round(t * .033), t),
    row('ExampleApp.AutosaveController.flush() -> ()', 'ExampleApp', Math.round(t * .022), t),
    row('CA::Transaction::commit()', 'QuartzCore', Math.round(t * .018), t),
  ].slice(0, limit);
  return { name: 'main', samples: n, busyPercent: 38.6, selfTop: self, inclusiveTop: incl, hotStacks: stacks };
}

function otherThreads(n, limit) {
  const g = n;
  return [
    { name: 'unnamed thread 1467899', samples: g, busyPercent: 100,
      selfTop: [row('iokit_user_client_trap', 'IOKit', g - 3, g), row('mach_msg2_trap', 'libsystem_kernel.dylib', 3, g)].slice(0, limit),
      inclusiveTop: [row('IOGPUCommandQueueSubmitCommandBuffers', 'IOGPU', g - 3, g), row('-[_MTLCommandQueue _submitAvailableCommandBuffers]', 'Metal', g - 3, g)].slice(0, limit),
      hotStacks: [stack(g - 3, g, ['_dispatch_workloop_worker_thread', '-[_MTLCommandQueue _submitAvailableCommandBuffers]', 'IOGPUCommandQueueSubmitCommandBuffers', 'iokit_user_client_trap'])].slice(0, limit) },
    { name: 'com.example.render', samples: g, busyPercent: 21.4,
      selfTop: [
        row('VoxelRender.GeometryStore.buildDrawList(viewFrustums: Swift.Array<Swift.Array<Swift.SIMD4<Swift.Float>>>) -> ()', 'ExampleApp', Math.round(g * .094), g),
        row('VoxelRender.SunShadowPass.cull(cascade: Swift.Int, frustum: VoxelRender.Frustum) -> Swift.Int', 'ExampleApp', Math.round(g * .051), g),
        row('-[MTLDebugRenderCommandEncoder drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:]', 'MetalTools', Math.round(g * .03), g),
        row('cp_frame_query_drawable', 'CompositorServices', Math.round(g * .022), g),
      ].slice(0, limit),
      inclusiveTop: [
        row('RenderCore.CompositorFrameLoop.renderFrame()', 'ExampleApp', Math.round(g * .214), g),
        row('VoxelRender.VoxelRenderer.encode(frame:)', 'ExampleApp', Math.round(g * .19), g),
        row('VoxelRender.SunShadowPass.encode(frame:)', 'ExampleApp', Math.round(g * .145), g),
      ].slice(0, limit),
      hotStacks: [
        stack(Math.round(g * .094), g, ['RenderCore.CompositorFrameLoop.renderLoop()', 'RenderCore.CompositorFrameLoop.renderFrame()', 'VoxelRender.VoxelRenderer.encode(frame:)', 'VoxelRender.SunShadowPass.encode(frame:)', 'VoxelRender.GeometryStore.buildDrawList(viewFrustums:)']),
        stack(Math.round(g * .051), g, ['RenderCore.CompositorFrameLoop.renderLoop()', 'RenderCore.CompositorFrameLoop.renderFrame()', 'VoxelRender.VoxelRenderer.encode(frame:)', 'VoxelRender.SunShadowPass.encode(frame:)', 'VoxelRender.SunShadowPass.cull(cascade:frustum:)']),
        stack(Math.round(g * .03), g, ['RenderCore.CompositorFrameLoop.renderLoop()', 'RenderCore.CompositorFrameLoop.renderFrame()', 'VoxelRender.VoxelRenderer.encode(frame:)', '-[MTLDebugRenderCommandEncoder drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:]']),
        stack(Math.round(g * .022), g, ['RenderCore.CompositorFrameLoop.renderLoop()', 'cp_layer_renderer_query_next_frame', 'cp_frame_query_drawable']),
      ].slice(0, limit) },
    { name: 'com.apple.audio.IOThread.client', samples: g, busyPercent: 6.2,
      selfTop: [row('RAVESpatialAudio.SoundStage.mix(into: Swift.UnsafeMutablePointer<Swift.Float>, frames: Swift.Int) -> ()', 'ExampleApp', Math.round(g * .04), g),
        row('vDSP_vsmul', 'Accelerate', Math.round(g * .018), g)].slice(0, limit),
      inclusiveTop: [row('PHASEEngine::RenderCallback', 'PHASE', Math.round(g * .062), g)].slice(0, limit),
      hotStacks: [stack(Math.round(g * .04), g, ['_pthread_start', 'HALC_ProxyIOContext::IOWorkLoop()', 'PHASEEngine::RenderCallback', 'RAVESpatialAudio.SoundStage.mix(into:frames:)'])].slice(0, limit) },
    { name: 'unnamed thread 1467897', samples: g, busyPercent: 3.9,
      selfTop: [row('VoxelRender.GeometryStore.buildDrawList(viewFrustums: Swift.Array<Swift.Array<Swift.SIMD4<Swift.Float>>>) -> ()', 'ExampleApp', 4, g)],
      inclusiveTop: [],
      hotStacks: [stack(3, g, ['RenderCore.CompositorFrameLoop.renderLoop()', 'RenderCore.CompositorFrameLoop.renderFrame()', 'VoxelRender.VoxelRenderer.encode(frame:)', 'VoxelRender.SunShadowPass.encode(frame:)', 'VoxelRender.GeometryStore.buildDrawList(viewFrustums:)'])] },
  ];
}

function profile(args) {
  const seconds = args.seconds ?? 2, intervalMs = args.intervalMs ?? 2, threads = args.threads ?? 'main', limit = args.limit ?? 12;
  if (typeof seconds !== 'number' || seconds < 0.2 || seconds > 10) return err(400, 'invalid_argument', `seconds must be between 0.2 and 10, got ${JSON.stringify(seconds)}`, 'pass seconds as a number from 0.2 to 10, e.g. {"seconds": 2}');
  if (typeof intervalMs !== 'number' || intervalMs < 0.5 || intervalMs > 50) return err(400, 'invalid_argument', `intervalMs must be between 0.5 and 50`, 'pass intervalMs as a number from 0.5 to 50');
  if (!['main', 'all'].includes(threads)) return err(400, 'invalid_argument', `threads must be "main" or "all"`, 'pass threads: "main" or "all"');
  if (!Number.isInteger(limit) || limit < 1 || limit > 50) return err(400, 'invalid_argument', 'limit must be an integer from 1 to 50', 'pass limit as an integer');
  const per = Math.round(seconds * 1000 / intervalMs * .887);
  // Busiest first, the way DebugProfiler sorts them.
  const list = threads === 'all'
    ? [...otherThreads(per, limit), mainThread(per, limit)].sort((a, b) => b.busyPercent - a.busyPercent)
    : [mainThread(per, limit)];
  return {
    delayMs: seconds * 1000,
    data: { seconds, intervalMs, samples: per * (threads === 'all' ? 13 : 1), overheadMs: Math.round(seconds * 37.7 * (threads === 'all' ? 1 : .2) * 10) / 10,
      ...(threads === 'all' ? { note: '3 quieter threads not shown' } : {}), threads: list },
  };
}

// ---- _memory: a slow upward drift, so snapshots have something to compare ----
let memTick = 0;
const KINDS = [
  ['IOAccelerator', 214, 1310 * MB, 905 * MB, 905 * MB],
  ['MALLOC_LARGE', 1820, 612 * MB, 401 * MB, 398 * MB],
  ['MALLOC_NANO', 6, 512 * MB, 38 * MB, 37 * MB],
  ['IOSurface', 48, 296 * MB, 251 * MB, 251 * MB],
  ['__TEXT', 612, 284 * MB, 96 * MB, 0],
  ['MALLOC_SMALL', 44, 176 * MB, 121 * MB, 119 * MB],
  ['VM_ALLOCATE', 391, 154 * MB, 72 * MB, 64 * MB],
  ['__DATA_CONST', 590, 61 * MB, 22 * MB, 3 * MB],
  ['CoreAnimation', 88, 58 * MB, 41 * MB, 41 * MB],
  ['Stack', 41, 44 * MB, 1.6 * MB, 1.6 * MB],
  ['__LINKEDIT', 1, 39 * MB, 18 * MB, 0],
  ['MALLOC_TINY', 9, 36 * MB, 14 * MB, 13 * MB],
  ['__DATA', 588, 24 * MB, 9 * MB, 8 * MB],
  ['Accelerate framework', 6, 18 * MB, 6 * MB, 6 * MB],
];
function memory(args) {
  const regions = args.regions ?? 12;
  if (!Number.isInteger(regions) || regions < 1 || regions > 60) return err(400, 'invalid_argument', 'regions must be an integer from 1 to 60', 'pass regions as an integer, e.g. {"regions": 12}');
  const k = memTick++;
  const grow = k * 23 * MB;
  const footprint = 1745 * MB + grow;
  const kinds = KINDS.map(([kind, n, v, r, d], i) => {
    const extra = i === 1 ? grow * .7 : i === 0 ? grow * .3 : 0;
    return { kind, regions: n + (i === 1 ? k * 37 : 0), virtualBytes: Math.round(v + extra * 1.2), residentBytes: Math.round(r + extra),
      dirtyBytes: Math.round(d + extra), swappedBytes: i === 2 ? 4 * MB : 0 };
  });
  return {
    data: {
      process: { footprintBytes: footprint, peakFootprintBytes: Math.max(footprint, 1810 * MB), residentBytes: 1490 * MB + grow,
        compressedBytes: 66 * MB, availableBytes: Math.max(0, 3072 * MB - footprint) },
      malloc: { blocksInUse: 412031 + k * 9100, sizeInUseBytes: 498 * MB + grow * .7, sizeAllocatedBytes: 640 * MB + grow * .8 },
      gpu: { currentAllocatedBytes: 1188 * MB + grow * .3, recommendedMaxWorkingSetBytes: 4864 * MB },
      largestRegionKinds: kinds.slice(0, regions), regionKindCount: KINDS.length + 17, sampledInMs: 41.7,
    },
  };
}

function info() {
  return { data: {
    app: { bundleId: 'com.example.ExampleApp', name: 'ExampleApp', version: '1.0', build: 'be18d4a', platform: 'visionOS', configuration: 'Debug', runningAs: null },
    device: { model: 'RealityDevice14,1', os: 'visionOS 27.2.0', processorCount: 8, physicalMemoryBytes: 16 * 1024 * MB, locale: 'en_US', timeZone: 'Europe/Vilnius' },
    process: { pid: 4127, launchedAt: new Date(Date.now() - 600000).toISOString(), uptimeSeconds: 600, memoryFootprintBytes: 1745 * MB, thermalState: 'nominal', lowPowerMode: false },
    trace: { session: 'S-31F0', signingKeyId: 'fake', uploadConfigured: true, subsystems: ['com.example.exampleapp'], privacy: 'development',
      logBuffer: { count: 1820, bytes: 412000, dropped: 0, capturesDebug: true } },
    capturedAt: new Date().toISOString(),
  } };
}

let seq = 0;
function logs(args) {
  const after = args.afterSequence ?? 0;
  const make = n => Array.from({ length: n }, () => {
    seq++;
    const lv = ['debug', 'info', 'info', 'notice', 'error'][seq % 5];
    return { time: new Date().toISOString(), level: lv, subsystem: 'com.example.exampleapp', category: ['World', 'Render', 'Portal', 'Input'][seq % 4],
      message: `frame ${seq}: ${['chunk streamed', 'draw list rebuilt', 'portal stencil updated', 'pinch began'][seq % 4]}`, sequence: seq };
  });
  if (after === 0) return { data: { entries: make(40), lastSequence: seq, truncated: false } };
  return { delayMs: Math.min((args.waitSeconds ?? 0) * 1000, 1500), data: () => ({ entries: make(2), lastSequence: seq, truncated: false }) };
}

function dispatch(name, args) {
  switch (name) {
    case '_info': return info();
    case '_profile': return profile(args);
    case '_memory': return memory(args);
    case '_logs': return logs(args);
    case '_features': return { data: { events: [{ name: 'portalEntered', at: new Date().toISOString() }] } };
    case 'renderStats': return { data: { fps: 89.7, frameMs: { p50: 10.9, p99: 14.2 }, drawCalls: 1412 } };
    case 'setWeather': return { data: { weather: args.kind } };
    case '_trace': return { data: { id: 'T1', upload: { accepted: false, response: { error: 'the fake app has no upload URL' } } } };
    default: return err(404, 'not_found', `no endpoint "${name}"`, 'GET / lists every endpoint');
  }
}

function createFakeApp({ token = null, approvalMs = 0, approval = 'allow' } = {}) {
  const allowed = new Set();
  const waiting = new Map();
  const server = http.createServer((q, s) => {
    let raw = '';
    q.on('data', c => { raw += c; });
    q.on('end', async () => {
      const send = (status, body) => { s.writeHead(status, { 'Content-Type': 'application/json' }); s.end(JSON.stringify(body)); };
      if (token && q.headers.authorization !== `Bearer ${token}`) {
        return send(401, { ok: false, error: { code: 'unauthenticated', message: 'this build requires its debug token', hint: 'send Authorization: Bearer <token>' } });
      }
      const client = q.headers['x-debugtrace-client'] || q.headers['user-agent'] || 'unknown';
      const url = new URL(q.url, 'http://x');
      if (url.pathname === '/' && q.method === 'GET') {
        return send(200, { service: 'debugtrace/1', app: info().data.app, endpoints: ENDPOINTS });
      }
      if (approvalMs > 0 && !allowed.has(client)) {
        if (!waiting.has(client)) waiting.set(client, new Promise(r => setTimeout(r, approvalMs)));
        await waiting.get(client);
        if (approval !== 'allow') {
          waiting.delete(client);
          return send(403, { ok: false, error: { code: 'forbidden', message: `waiting for someone to allow debug access for "${client}" on the device`,
            hint: 'accept the prompt in the app (it must be in the foreground), then retry' } });
        }
        allowed.add(client);
      }
      let name, args;
      if (url.pathname === '/_call') {
        try { ({ name, arguments: args } = JSON.parse(raw || '{}')); } catch { return send(400, { ok: false, error: { code: 'invalid_argument', message: 'body is not JSON', hint: 'send JSON' } }); }
      } else {
        name = url.pathname.slice(1);
        args = raw ? JSON.parse(raw) : Object.fromEntries(url.searchParams);
      }
      const started = Date.now();
      const r = dispatch(name, args || {});
      if (r.delayMs) await new Promise(res => setTimeout(res, r.delayMs));
      if (r.status) return send(r.status, r.body);
      send(200, { ok: true, endpoint: name, elapsedMs: Date.now() - started, data: typeof r.data === 'function' ? r.data() : r.data });
    });
  });
  return server;
}

module.exports = { createFakeApp, profile, memory };
