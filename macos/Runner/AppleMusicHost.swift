import AppKit
import Combine
import Foundation
@preconcurrency import FlutterMacOS
import MusicKit

/// Plays Apple Music for the hub's hosted player with MusicKit.
///
/// Dart only forwards. Each batch of commands arrives as JSON text (`batch`), and every event
/// goes back as JSON text through an `event` call. `authorize` and `status` are separate calls.
/// The player needs macOS 14; on older systems the host reports that it cannot play.
@MainActor
final class AppleMusicHost {
  static let shared = AppleMusicHost()

  private var channel: FlutterMethodChannel?
  /// The `MusicKitPlayback`, which only exists on macOS 14 and later.
  private var playback: AnyObject?

  private var lastAuthorized: Bool?
  private var activeObserver: NSObjectProtocol?

  private init() {}

  /// Creates the channel on the engine's messenger. Called once the Flutter view controller exists.
  func attach(messenger: FlutterBinaryMessenger) {
    let channel = FlutterMethodChannel(
      name: "ci.not.rune/apple_music", binaryMessenger: messenger)
    channel.setMethodCallHandler { [weak self] (call, result) in
      // Flutter delivers channel calls on the main thread.
      MainActor.assumeIsolated {
        guard let self else {
          result(FlutterMethodNotImplemented)
          return
        }
        self.handle(call, result)
      }
    }
    self.channel = channel
    lastAuthorized = Self.isAuthorized

    // Access can be changed in System Settings while Rune runs.
    activeObserver = NotificationCenter.default.addObserver(
      forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
    ) { [weak self] _ in
      MainActor.assumeIsolated {
        guard let self, Self.isAuthorized != self.lastAuthorized else { return }
        self.lastAuthorized = Self.isAuthorized
        Task {
          let ready = await Self.hostReady()
          self.channel?.invokeMethod("event", arguments: ready)
        }
      }
    }
  }

  fileprivate static var isAuthorized: Bool {
    if #available(macOS 14, *) {
      return MusicAuthorization.currentStatus == .authorized
    }
    return false
  }

  private func handle(_ call: FlutterMethodCall, _ result: @escaping FlutterResult) {
    switch call.method {
    case "batch":
      guard let json = call.arguments as? String else {
        result(FlutterError(code: "bad_arguments", message: "A batch is JSON text", details: nil))
        return
      }
      if #available(macOS 14, *) {
        player().submit(json)
      }
      result(nil)
    case "status":
      Task { result(await Self.hostReady()) }
    case "authorize":
      let interactive = (call.arguments as? [String: Any])?["interactive"] as? Bool ?? false
      Task {
        let reply = await Self.authorize(interactive: interactive)
        self.lastAuthorized = Self.isAuthorized
        // The player's own reports follow the authorization, so Dart hears about it either way.
        let ready = await Self.hostReady()
        self.channel?.invokeMethod("event", arguments: ready)
        result(reply)
      }
    default:
      result(FlutterMethodNotImplemented)
    }
  }

  @available(macOS 14, *)
  private func player() -> MusicKitPlayback {
    if let existing = playback as? MusicKitPlayback {
      return existing
    }
    let created = MusicKitPlayback { [weak self] json in
      self?.channel?.invokeMethod("event", arguments: json)
    }
    playback = created
    return created
  }

  /// The `hostReady` event for the current authorization. It never asks the user for anything.
  fileprivate static func hostReady(canPlay forced: Bool? = nil) async -> String {
    // Before macOS 14 there is nothing to sign in to, so Rune is not asked to.
    var authorized = true
    var canPlay = false
    if #available(macOS 14, *) {
      authorized = MusicAuthorization.currentStatus == .authorized
      // Asking MusicKit about the subscription before access is granted leaves it in a cached
      // error state, so only ask once authorized.
      if authorized {
        canPlay = (try? await MusicSubscription.current.canPlayCatalogContent) ?? false
      }
    }
    return encode([
      "type": "hostReady",
      "host": "appleMusic",
      "authorized": authorized,
      "canPlay": forced ?? canPlay,
    ])
  }

  /// Authorizes and fetches the tokens the hub needs. The system's consent dialog only shows when
  /// `interactive` is set.
  private static func authorize(interactive: Bool) async -> [String: Any] {
    guard #available(macOS 14, *) else { return ["authorized": false] }
    if interactive, MusicAuthorization.currentStatus != .authorized {
      _ = await MusicAuthorization.request()
    }
    guard MusicAuthorization.currentStatus == .authorized else {
      return ["authorized": false]
    }
    var reply: [String: Any] = ["authorized": true]
    do {
      // Asking before authorization puts MusicKit into a cached XPC error state.
      let provider = DefaultMusicTokenProvider()
      let developerToken = try await provider.developerToken(options: [])
      let userToken = try await provider.userToken(for: developerToken, options: [])
      reply["token"] = userToken
      reply["developerToken"] = developerToken
    } catch {
      // The tokens are left out; Dart treats that as not signed in. Never log the tokens.
      NSLog("%@", "Apple Music host: fetching the tokens failed: \(type(of: error))")
    }
    return reply
  }
}

private func encode(_ object: [String: Any]) -> String {
  guard let data = try? JSONSerialization.data(withJSONObject: object),
    let text = String(data: data, encoding: .utf8)
  else {
    return "{}"
  }
  return text
}

/// One action of a batch. Which fields are set depends on `action`.
private struct WireAction: Decodable, Sendable {
  let action: String
  let ids: [String]?
  let index: Int?
  let offsetMs: Double?
  let gain: Double?
}

private struct WireBatch: Decodable, Sendable {
  let revision: UInt64
  let actions: [WireAction]
}

/// Hands one result to whoever waits, once: the work, the timeout or a cancel, whichever is first.
private final class Gate<T: Sendable>: @unchecked Sendable {
  private let lock = NSLock()
  private var result: Result<T, Error>?
  private var continuation: CheckedContinuation<T, Error>?

  func resolve(_ value: Result<T, Error>) {
    lock.lock()
    guard result == nil else {
      lock.unlock()
      return
    }
    result = value
    let waiting = continuation
    continuation = nil
    lock.unlock()
    waiting?.resume(with: value)
  }

  func wait() async throws -> T {
    try await withCheckedThrowingContinuation { (waiting: CheckedContinuation<T, Error>) in
      lock.lock()
      if let result {
        lock.unlock()
        waiting.resume(with: result)
      } else {
        continuation = waiting
        lock.unlock()
      }
    }
  }
}

private struct TimedOut: Error {}

/// The hosted player's protocol on top of `ApplicationMusicPlayer`, one track at a time.
///
/// Batches run one after another in the order they arrive. Every event carries the revision of
/// the latest batch that was applied. A batch with `set` or `clear` makes every older batch that
/// has not finished give up, and cancels the lookup in flight, so the newest pick wins.
@available(macOS 14, *)
@MainActor
final class MusicKitPlayback {
  // MusicKit does not mark its player Sendable, and its async methods run off the main actor.
  // Everything here is driven from the main actor, one batch at a time.
  nonisolated(unsafe) private let player = ApplicationMusicPlayer.shared
  private let send: (String) -> Void
  private static let timeout: Double = 20

  /// Songs resolved earlier, so a replay does not ask again.
  private var songs: [String: Song] = [:]
  private var tail: Task<Void, Never>?
  private var revision: UInt64 = 0
  /// Counts the batches that replace the track. A batch is stale once a newer one was queued.
  private var pickEpoch = 0
  private var cancelInflight: (() -> Void)?

  private var song: Song?
  /// Whether the player holds the track Rune set.
  private var hasTrack = false
  /// Whether that track finished by itself.
  private var ended = false
  /// Whether a failure of that track was reported.
  private var failed = false
  /// Whether the track was heard playing since the last start.
  private var sawPlaying = false
  /// The furthest position seen, since MusicKit may reset the time when the track ends.
  private var furthest: Double = 0
  /// The position a `skip` asked for, kept only for a `start` in the same batch.
  private var pendingOffset: Double?
  /// Events are held back while a batch is being applied.
  private var applying = false
  private var reportedSignature = ""

  private var subscriptions: Set<AnyCancellable> = []
  private var timer: Timer?

  init(send: @escaping (String) -> Void) {
    self.send = send

    player.state.objectWillChange
      .receive(on: DispatchQueue.main)
      .sink { [weak self] _ in self?.check(periodic: false) }
      .store(in: &subscriptions)

    // The hub does not interpolate the position, so it is reported twice a second while playing.
    // The common modes keep the timer running during window resizes and open menus.
    let timer = Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in
      MainActor.assumeIsolated { self?.check(periodic: true) }
    }
    RunLoop.main.add(timer, forMode: .common)
    self.timer = timer

    // A subscription that starts or ends changes what can be played.
    Task { [weak self] in
      for await _ in MusicSubscription.subscriptionUpdates {
        guard let self else { return }
        self.send(await AppleMusicHost.hostReady())
      }
    }
  }

  private var isPlaying: Bool {
    switch player.state.playbackStatus {
    case .playing, .seekingForward, .seekingBackward: return true
    default: return false
    }
  }

  // MARK: - Batches

  /// Queues one batch, given as JSON text.
  func submit(_ json: String) {
    guard let data = json.data(using: .utf8),
      let batch = try? JSONDecoder().decode(WireBatch.self, from: data)
    else {
      NSLog("%@", "Apple Music host: unreadable batch")
      return
    }
    if batch.actions.contains(where: { $0.action == "set" || $0.action == "clear" }) {
      pickEpoch += 1
      cancelInflight?()
    }
    let epoch = pickEpoch
    let previous = tail
    tail = Task { [weak self] in
      await previous?.value
      await self?.run(batch, epoch: epoch)
    }
  }

  private func run(_ batch: WireBatch, epoch: Int) async {
    guard epoch == pickEpoch else { return }
    revision = batch.revision
    applying = true
    var replaced = false
    for action in batch.actions {
      // A newer pick came in while this batch was running.
      guard epoch == pickEpoch else {
        applying = false
        return
      }
      switch action.action {
      case "set":
        replaced = true
        await set(action.ids?.first, epoch: epoch)
      case "skip":
        skip(to: max(0, (action.offsetMs ?? 0) / 1000))
      case "setGain":
        // MusicKit's player has no volume of its own on macOS; the system volume applies.
        break
      case "start":
        await start()
      case "stop":
        player.pause()
      case "clear":
        replaced = true
        clear()
      case "status":
        break
      default:
        // add and remove are not supported: Rune holds one track at a time.
        break
      }
    }
    pendingOffset = nil
    guard epoch == pickEpoch else {
      applying = false
      return
    }
    // Look at the player before answering, so an end is seen once and before the status.
    let wasEnded = ended
    applying = false
    reconcile()
    // A batch about a track that already ended still needs its answer at this revision.
    if wasEnded, !replaced, hasTrack {
      send(encode(["type": "jukeboxEnded", "host": "appleMusic", "revision": revision]))
    }
    sendStatus()
  }

  private func clear() {
    player.stop()
    player.queue = []
    song = nil
    hasTrack = false
    ended = false
    failed = false
    sawPlaying = false
    furthest = 0
    pendingOffset = nil
  }

  private func skip(to seconds: Double) {
    guard hasTrack else { return }
    if ended, let duration = song?.duration, seconds < duration - 1 {
      // Seeking back into a finished track plays it again.
      ended = false
      failed = false
    }
    player.playbackTime = seconds
    furthest = seconds
    // Playback that has not begun yet may drop the seek, so a start in this batch repeats it.
    pendingOffset = isPlaying ? nil : seconds
  }

  private func set(_ id: String?, epoch: Int) async {
    clear()
    guard let id else { return }
    guard MusicAuthorization.currentStatus == .authorized else {
      failTrack(.notAuthorized)
      return
    }
    guard let resolved = await resolve(id), epoch == pickEpoch else { return }
    song = resolved
    player.queue = ApplicationMusicPlayer.Queue(for: [resolved])
    hasTrack = true
    await attempt(epoch: epoch) { try await $0.prepareToPlay() }
  }

  private func start() async {
    guard hasTrack, !ended, !failed else { return }
    sawPlaying = false
    await attempt(epoch: pickEpoch) { try await $0.play() }
    if let offset = pendingOffset {
      player.playbackTime = offset
      pendingOffset = nil
    }
  }

  // MARK: - MusicKit calls

  /// Runs `work` for at most `timeout` seconds, or until a newer pick cancels it. The work keeps
  /// running if MusicKit ignores the cancel, but nobody waits for it any more.
  private func bounded<T: Sendable>(
    _ work: @escaping @Sendable () async throws -> T
  ) async throws -> T {
    let gate = Gate<T>()
    let worker = Task {
      do {
        gate.resolve(.success(try await work()))
      } catch {
        gate.resolve(.failure(error))
      }
    }
    let watchdog = Task {
      try? await Task.sleep(nanoseconds: UInt64(Self.timeout * 1_000_000_000))
      gate.resolve(.failure(TimedOut()))
      worker.cancel()
    }
    cancelInflight = {
      gate.resolve(.failure(CancellationError()))
      worker.cancel()
    }
    defer {
      watchdog.cancel()
      cancelInflight = nil
    }
    return try await gate.wait()
  }

  /// Runs one player call; a failure is reported unless it only means a newer pick took over.
  private func attempt(
    epoch: Int, _ work: @escaping @Sendable (ApplicationMusicPlayer) async throws -> Void
  ) async {
    nonisolated(unsafe) let player = self.player
    do {
      try await bounded { try await work(player) }
    } catch {
      if Self.isCancellation(error) || epoch != pickEpoch { return }
      NSLog("%@", "Apple Music host: playback failed: \(error)")
      failure(of: error)
    }
  }

  /// A queue that replaces one that is still starting interrupts its `play()`; that is routine.
  private static func isCancellation(_ error: Error) -> Bool {
    if error is CancellationError { return true }
    let nsError = error as NSError
    return nsError.domain == "MPMusicPlayerControllerErrorDomain" && nsError.code == 2
  }

  /// Reports a failed MusicKit call. `play()` surfaces MediaPlayer errors, so MPErrorDomain is
  /// read as well as the URL and MusicKit errors.
  private func failure(of error: Error) {
    if error is TimedOut {
      failTrack(.network)
      return
    }
    if MusicAuthorization.currentStatus != .authorized {
      failTrack(.notAuthorized)
      return
    }
    let nsError = error as NSError
    switch (nsError.domain, nsError.code) {
    case (NSURLErrorDomain, _), ("MPErrorDomain", 3), ("MPErrorDomain", 7):
      failTrack(.network)
    case ("MPErrorDomain", 1):
      failTrack(.notAuthorized)
    case ("MPErrorDomain", 2):
      // The subscription lapsed, so nothing can be played any more.
      failTrack(.unplayable)
      Task { self.send(await AppleMusicHost.hostReady(canPlay: false)) }
    default:
      if let sub = error as? MusicSubscription.Error, sub == .permissionDenied {
        failTrack(.notAuthorized)
      } else if let library = error as? MusicLibrary.Error, library == .permissionDenied {
        failTrack(.notAuthorized)
      } else {
        failTrack(.unplayable)
      }
    }
  }

  // MARK: - Songs

  /// The song for `id`: `i.…` is a library song, `catalog:<id>` a catalogue song. A song that
  /// cannot be found is reported as unplayable.
  private func resolve(_ id: String) async -> Song? {
    if let cached = songs[id] { return cached }
    do {
      let found: Song? = try await bounded {
        if let catalogID = id.removingPrefix("catalog:") {
          let request = MusicCatalogResourceRequest<Song>(
            matching: \.id, equalTo: MusicItemID(catalogID))
          return try await request.response().items.first
        }
        var request = MusicLibraryRequest<Song>()
        request.filter(matching: \.id, equalTo: MusicItemID(id))
        return try await request.response().items.first
      }
      guard let song = found else {
        failTrack(.unplayable)
        return nil
      }
      if songs.count > 2000 { songs.removeAll() }
      songs[id] = song
      return song
    } catch {
      if Self.isCancellation(error) { return nil }
      NSLog("%@", "Apple Music host: looking up a song failed: \(error)")
      failure(of: error)
      return nil
    }
  }

  // MARK: - Events

  private enum Failure: String {
    case unplayable
    case notAuthorized
    case network
  }

  /// Reports a failure of the current track, once.
  private func failTrack(_ reason: Failure) {
    guard !failed else { return }
    failed = true
    send(
      encode([
        "type": "jukeboxItemFailed",
        "host": "appleMusic",
        "revision": revision,
        "index": 0,
        "reason": reason.rawValue,
      ]))
  }

  private var position: Double {
    let time = max(0, player.playbackTime)
    return ended ? max(time, furthest) : time
  }

  private func sendStatus() {
    reportedSignature = signature
    send(
      encode([
        "type": "jukeboxStatus",
        "host": "appleMusic",
        "revision": revision,
        "currentIndex": hasTrack ? (0 as Any) : (NSNull() as Any),
        "playing": hasTrack && !ended && isPlaying,
        "positionMs": Int((position * 1000).rounded()),
      ]))
  }

  /// What Rune can see of the player, so a change it did not cause is noticed.
  private var signature: String {
    "\(hasTrack)|\(ended)|\(failed)|\(player.state.playbackStatus)"
  }

  /// Notices the track ending or failing, and reports a change of state that Rune did not cause.
  /// Nothing is sent while a batch is applied: the batch answers once it is done.
  private func check(periodic: Bool) {
    guard !applying else { return }
    reconcile()
    guard hasTrack else { return }
    if signature != reportedSignature || (periodic && isPlaying) {
      sendStatus()
    }
  }

  private func reconcile() {
    guard !applying, hasTrack, !ended, !failed else { return }
    let time = max(0, player.playbackTime)
    if isPlaying {
      sawPlaying = true
      furthest = max(furthest, time)
      return
    }
    let status = player.state.playbackStatus
    let reached = max(time, furthest)
    if let duration = song?.duration, duration > 0 {
      // Playing out ends the track whether MusicKit then shows stopped or paused.
      if sawPlaying, reached >= duration - 1 {
        finish()
      } else if sawPlaying, status == .stopped {
        // Stopped well before the end, with no stop or clear from Rune.
        failure(of: PlaybackStopped())
      }
    } else if sawPlaying, status == .stopped {
      finish()
    }
  }

  private struct PlaybackStopped: Error {}

  private func finish() {
    ended = true
    send(encode(["type": "jukeboxEnded", "host": "appleMusic", "revision": revision]))
    sendStatus()
  }
}

extension String {
  nonisolated fileprivate func removingPrefix(_ prefix: String) -> String? {
    hasPrefix(prefix) ? String(dropFirst(prefix.count)) : nil
  }
}
