import AppKit
import Combine
import Foundation
@preconcurrency import FlutterMacOS
import MusicKit

/// Plays Apple Music for the hub's hosted player with MusicKit.
///
/// Dart only forwards. Each batch of commands arrives as JSON text (`batch`), and every event
/// goes back as JSON text through an `event` call. `authorize` and `status` are separate calls:
/// Dart attaches with `status`, whose answer is the host's `hostReady`. The player needs macOS
/// 14; on older systems the host reports that it cannot play.
@MainActor
final class AppleMusicHost {
  static let shared = AppleMusicHost()

  private var channel: FlutterMethodChannel?
  /// The `MusicKitPlayback`, which only exists on macOS 14 and later.
  private var playback: AnyObject?

  /// What the latest `hostReady` was based on, so that only a change is reported again.
  private var lastAuthorized: Bool?
  private var lastCanPlay: Bool?
  private var activeObserver: NSObjectProtocol?
  private var subscriptionWatch: Task<Void, Never>?

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
        Task { await self.restart() }
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
      Task {
        let canPlay = await self.currentCanPlay()
        // Dart attaches with this call, and the player takes the answer for a host that just
        // started. Whatever is still held from before goes in the same turn as the answer.
        self.dropQueue()
        result(self.readyEvent(canPlay: canPlay))
      }
    case "authorize":
      let interactive = (call.arguments as? [String: Any])?["interactive"] as? Bool ?? false
      Task {
        let reply = await Self.authorize(interactive: interactive)
        // A change of access restarts the host. Without one the host keeps what it holds: a
        // `hostReady` would make the player hand its track over again.
        if Self.isAuthorized != self.lastAuthorized {
          self.lastAuthorized = Self.isAuthorized
          await self.restart()
        }
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
    let created = MusicKitPlayback(
      send: { [weak self] json in
        self?.channel?.invokeMethod("event", arguments: json)
      },
      lapsed: { [weak self] in
        // The playback dropped its queue already, in this same turn.
        guard let self else { return }
        self.channel?.invokeMethod("event", arguments: self.readyEvent(canPlay: false))
      })
    playback = created
    return created
  }

  /// Reports the host as started again. It drops its queue and says so in the same turn, so that
  /// no batch comes in between, since the player then hands the current track over again.
  private func restart(canPlay known: Bool? = nil) async {
    let canPlay: Bool
    if let known {
      canPlay = known
    } else {
      canPlay = await currentCanPlay()
    }
    dropQueue()
    channel?.invokeMethod("event", arguments: readyEvent(canPlay: canPlay))
  }

  private func dropQueue() {
    if #available(macOS 14, *) {
      (playback as? MusicKitPlayback)?.drop()
    }
  }

  /// Whether the catalogue can be played. It never asks the user for anything.
  private func currentCanPlay() async -> Bool {
    guard #available(macOS 14, *), MusicAuthorization.currentStatus == .authorized else {
      return false
    }
    // Asking MusicKit about the subscription before access is granted leaves it in a cached
    // error state, so only ask once authorized.
    return (try? await MusicSubscription.current.canPlayCatalogContent) ?? false
  }

  /// The `hostReady` event for the current authorization, which it records.
  private func readyEvent(canPlay: Bool) -> String {
    // Before macOS 14 there is nothing to sign in to, so Rune is not asked to.
    var authorized = true
    if #available(macOS 14, *) {
      authorized = MusicAuthorization.currentStatus == .authorized
      if authorized {
        watchSubscription()
      }
    }
    lastAuthorized = Self.isAuthorized
    lastCanPlay = canPlay
    return encode([
      "type": "hostReady",
      "host": "appleMusic",
      "authorized": authorized,
      "canPlay": canPlay,
    ])
  }

  /// A subscription that starts or ends changes what can be played.
  @available(macOS 14, *)
  private func watchSubscription() {
    guard subscriptionWatch == nil else { return }
    subscriptionWatch = Task { [weak self] in
      for await subscription in MusicSubscription.subscriptionUpdates {
        guard let self else { return }
        let canPlay = subscription.canPlayCatalogContent
        if MusicAuthorization.currentStatus == .authorized, canPlay != self.lastCanPlay {
          await self.restart(canPlay: canPlay)
        }
      }
    }
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
nonisolated private final class Gate<T: Sendable>: @unchecked Sendable {
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

nonisolated private struct TimedOut: Error {}

nonisolated private struct PlaybackStopped: Error {}

private enum Failure: String {
  case unplayable
  case notAuthorized
  case network
}

/// An entry of the host's queue: the id the player gave, and once found, its song and whether
/// MusicKit's queue holds it.
@available(macOS 14, *)
private final class Entry {
  let id: String
  var song: Song?
  /// Whether the entry went into MusicKit's queue, by a new queue or an insert.
  var queued = false
  /// The id of the MusicKit queue entry that plays it. MusicKit may not give one at once, or give
  /// the entry a new one, and the song then tells which queue entry it is.
  var queueEntryID: String?
  var failure: Failure?
  var lookup: Task<Void, Never>?

  init(id: String) {
    self.id = id
  }

  var failed: Bool { failure != nil }
}

/// The hosted player's protocol on top of `ApplicationMusicPlayer`.
///
/// The host's queue holds the entries the player gave since its latest `set`, and indexes count
/// from 0 there. MusicKit's queue gets an entry once its song is found, by an insert at its
/// tail that leaves what plays alone. A host entry knows its MusicKit queue entry by that entry's
/// id, or else by its song. MusicKit's current entry changing to the next entry's is the host
/// moving on; the last entry playing out with nothing playable after it is the end.
///
/// Batches and queue edits run one after another in the order they arrive. Every event carries
/// the revision of the latest batch that was applied. A batch with `set` or `clear` makes every
/// batch, lookup and insert queued before it stale, and cancels the MusicKit calls they wait
/// for, so the newest pick wins and nothing older touches its queue.
@available(macOS 14, *)
@MainActor
final class MusicKitPlayback {
  private let player = ApplicationMusicPlayer.shared
  private let send: (String) -> Void
  /// The subscription lapsed, and the queue was dropped: the host can play nothing any more.
  private let lapsed: () -> Void

  /// How long a lookup may take outside a batch.
  private static let timeout: Duration = .seconds(20)
  /// How long an insert into MusicKit's queue may hold up the batches queued behind it.
  private static let insertTimeout: Duration = .seconds(5)
  /// The player counts a track as unreachable when its host does not report within 10 s of
  /// being told to play it. A batch gives up on MusicKit sooner, so it reports that itself.
  private static let batchBudget: Duration = .seconds(9)
  /// How long an entry that was told to play may stay silent before it fails.
  private static let startLimit: Duration = .seconds(8)
  /// How often a status goes out while a track plays.
  private static let reportEvery: Duration = .seconds(2)

  /// Songs resolved earlier, so a replay does not ask again.
  private var songs: [String: Song] = [:]

  private var tail: Task<Void, Never>?
  /// The batches and queue edits that are queued or running, so a newer pick can cancel them.
  private var running: [Int: Task<Void, Never>] = [:]
  private var nextJob = 0
  /// Counts the picks: `set`, `clear`, and the host dropping its queue. Work queued for an
  /// older pick is stale.
  private var epoch = 0
  private var revision: UInt64 = 0
  /// Events are held back while a batch is being applied; the batch answers once it is done.
  private var applying = false
  /// Whether a lapsed subscription was reported since the latest `set`.
  private var lapseReported = false

  private var entries: [Entry] = []
  private var current: Int?
  /// Whether the last entry finished with nothing playable after it.
  private var ended = false
  /// Whether Rune wants the current entry to play.
  private var wantPlaying = false
  /// When Rune last paused, since MusicKit may still say playing just after.
  private var stoppedAt = ContinuousClock.now
  /// When the current entry was told to play and has not been heard playing since.
  private var startedAt: ContinuousClock.Instant?
  /// Whether the current entry was heard playing since it became current or was last started.
  private var sawPlaying = false
  /// The furthest position seen, since MusicKit may reset the time when an entry ends.
  private var furthest: Double = 0
  /// When the current entry was first seen played out while an entry follows it.
  private var stalledSince: ContinuousClock.Instant?
  /// Whether the host moves on to the next entry itself, as MusicKit did not.
  private var advancing = false
  /// The position a `skip` asked for, kept only for a `start` in the same batch.
  private var pendingOffset: Double?
  /// The MusicKit queue entry last found to map to no track, so that it is logged once.
  private var unmatchedEntryID: String?

  private var reportedSignature = ""
  private var reportedAt = ContinuousClock.now
  private var subscriptions: Set<AnyCancellable> = []
  private var queueSubscription: AnyCancellable?
  private var timer: Timer?

  init(send: @escaping (String) -> Void, lapsed: @escaping () -> Void) {
    self.send = send
    self.lapsed = lapsed

    // The player keeps the play order and its modes, so MusicKit plays the queue as it is.
    player.state.repeatMode = MusicPlayer.RepeatMode.none
    player.state.shuffleMode = MusicPlayer.ShuffleMode.off

    player.state.objectWillChange
      .receive(on: DispatchQueue.main)
      .sink { [weak self] _ in self?.check(periodic: false) }
      .store(in: &subscriptions)
    observeQueue()

    // The common modes keep the timer running during window resizes and open menus.
    let timer = Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in
      MainActor.assumeIsolated { self?.check(periodic: true) }
    }
    RunLoop.main.add(timer, forMode: .common)
    self.timer = timer
  }

  /// The queue object is replaced when a new queue is set, so its subscription is renewed then.
  private func observeQueue() {
    queueSubscription = player.queue.objectWillChange
      .receive(on: DispatchQueue.main)
      .sink { [weak self] _ in self?.check(periodic: false) }
  }

  private var isPlaying: Bool {
    switch player.state.playbackStatus {
    case .playing, .seekingForward, .seekingBackward: return true
    default: return false
    }
  }

  private var currentEntry: Entry? {
    guard let current, entries.indices.contains(current) else { return nil }
    return entries[current]
  }

  private var nextEntry: Entry? {
    guard let current, entries.indices.contains(current + 1) else { return nil }
    return entries[current + 1]
  }

  private func index(of entry: Entry) -> Int? {
    entries.firstIndex { $0 === entry }
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
    let deadline = ContinuousClock.now + Self.batchBudget
    if batch.actions.contains(where: { $0.action == "set" || $0.action == "clear" }) {
      supersede()
    }
    let epoch = epoch
    enqueue { [weak self] in await self?.run(batch, epoch: epoch, deadline: deadline) }
  }

  /// Stops and empties the queue, as a host that starts holds nothing.
  func drop() {
    supersede()
    reset()
  }

  /// Makes everything queued so far stale, and stops the lookups and MusicKit calls it waits for.
  private func supersede() {
    epoch += 1
    for task in running.values {
      task.cancel()
    }
    running.removeAll()
    for entry in entries {
      entry.lookup?.cancel()
    }
  }

  /// Runs `job` after everything queued before it.
  private func enqueue(_ job: @escaping @MainActor () async -> Void) {
    let key = nextJob
    nextJob += 1
    let previous = tail
    let task = Task { [weak self] in
      await previous?.value
      await job()
      self?.running[key] = nil
    }
    running[key] = task
    tail = task
  }

  private func run(_ batch: WireBatch, epoch: Int, deadline: ContinuousClock.Instant) async {
    guard epoch == self.epoch else { return }
    revision = batch.revision
    applying = true
    defer { applying = false }
    let endedBefore = ended
    var picked = false
    for action in batch.actions {
      // A newer pick came in while this batch was running.
      guard epoch == self.epoch else { return }
      // MusicKit may have moved on by itself since the last look.
      followMusicKit()
      switch action.action {
      case "set":
        picked = true
        await set(action.ids ?? [], epoch: epoch, deadline: deadline)
      case "add":
        add(action.ids ?? [], epoch: epoch)
      case "remove":
        remove(at: action.index ?? -1)
      case "clear":
        picked = true
        reset()
      case "skip":
        await skip(
          to: action.index ?? -1, at: max(0, (action.offsetMs ?? 0) / 1000), epoch: epoch,
          deadline: deadline)
      case "start":
        await start(epoch: epoch, deadline: deadline)
      case "stop":
        stop()
      case "setGain":
        // MusicKit's player has no volume of its own on macOS; the system volume applies.
        break
      default:
        break
      }
    }
    pendingOffset = nil
    guard epoch == self.epoch else { return }
    applying = false
    // Look at the player before answering, so an end is seen once and before the status.
    reconcile()
    // A batch that comes after the queue ended still needs its answer at this revision.
    if endedBefore, ended, !picked {
      sendEnded()
    }
    sendStatus()
  }

  /// Stops MusicKit and forgets every entry.
  private func reset() {
    for entry in entries {
      entry.lookup?.cancel()
    }
    player.stop()
    player.queue = []
    observeQueue()
    entries = []
    current = nil
    ended = false
    wantPlaying = false
    startedAt = nil
    sawPlaying = false
    furthest = 0
    stalledSince = nil
    advancing = false
    pendingOffset = nil
  }

  /// The current song goes to MusicKit as soon as it is found; the next one is looked up after
  /// it and joins MusicKit's queue in place once found.
  private func set(_ ids: [String], epoch: Int, deadline: ContinuousClock.Instant) async {
    reset()
    lapseReported = false
    entries = ids.map { Entry(id: $0) }
    guard let first = entries.first else { return }
    current = 0
    guard MusicAuthorization.currentStatus == .authorized else {
      fail(first, .notAuthorized)
      return
    }
    guard let song = await resolve(first, epoch: epoch, deadline: deadline) else { return }
    replaceQueue(startingWith: first, song)
    for entry in entries.dropFirst() {
      lookUp(entry, epoch: epoch)
    }
    await attempt(on: first, epoch: epoch, deadline: deadline) {
      try await self.player.prepareToPlay()
    }
  }

  /// Appends entries. Nothing starts, and after the end nothing is looked up until a `skip`
  /// makes an entry current.
  private func add(_ ids: [String], epoch: Int) {
    for id in ids {
      let entry = Entry(id: id)
      entries.append(entry)
      if !ended {
        lookUp(entry, epoch: epoch)
      }
    }
  }

  /// Removes an entry after the current one, from MusicKit's queue too, and stops its lookup so
  /// that it is never inserted. A `remove` aimed at the current entry, as when the host moved on
  /// just as the player changed its next track, is ignored.
  private func remove(at index: Int) {
    guard let current, index > current, entries.indices.contains(index) else { return }
    let entry = entries.remove(at: index)
    entry.lookup?.cancel()
    if entry.queued {
      pruneQueue()
    }
  }

  /// Within the current entry this is a seek. To the next entry it moves MusicKit on in place,
  /// playing or paused as before.
  private func skip(
    to index: Int, at seconds: Double, epoch: Int, deadline: ContinuousClock.Instant
  ) async {
    guard let current, !ended, entries.indices.contains(index) else { return }
    if index == current {
      await seek(to: seconds, epoch: epoch, deadline: deadline)
      return
    }
    guard index == current + 1 else {
      NSLog("%@", "Apple Music host: ignoring a skip from entry \(current) to \(index)")
      return
    }
    let next = entries[index]
    let failedBefore = next.failed
    let song = await song(for: next, epoch: epoch, deadline: deadline)
    guard epoch == self.epoch, self.current == current, self.index(of: next) == index else {
      return
    }
    if let song, !next.failed {
      if stalledSince != nil || advancing {
        // The current entry played out, so a fresh queue costs no gap.
        replaceQueue(startingWith: next, song)
      } else {
        if !next.queued {
          await insert(next, song, epoch: epoch, deadline: deadline)
          guard epoch == self.epoch, self.current == current else { return }
        }
        if next.queued, !next.failed, !musicKitPlays(next) {
          // MusicKit may have moved on by itself as the skip failed.
          await attempt(on: next, epoch: epoch, deadline: deadline, unless: musicKitPlays(next)) {
            try await self.player.skipToNextEntry()
          }
          guard epoch == self.epoch, self.current == current else { return }
        }
      }
    }
    becomeCurrent(index, at: seconds)
    guard !next.failed else {
      // The player moved on to an entry this host cannot play: nothing plays, and the player
      // hears why at this revision.
      startedAt = nil
      player.pause()
      if failedBefore, let failure = next.failure {
        sendFailed(index, failure)
      }
      return
    }
    if seconds > 0 {
      player.playbackTime = seconds
      pendingOffset = isPlaying ? nil : seconds
    }
    if !wantPlaying {
      player.pause()
    }
  }

  /// Moves within the current entry. One that played out goes back to MusicKit first, playing
  /// or paused as before.
  private func seek(to seconds: Double, epoch: Int, deadline: ContinuousClock.Instant) async {
    guard let current, let entry = currentEntry, entry.queued, !entry.failed else { return }
    if stalledSince != nil || advancing, let song = entry.song {
      replaceQueue(startingWith: entry, song)
      becomeCurrent(current, at: seconds)
      if wantPlaying {
        await attempt(on: entry, epoch: epoch, deadline: deadline) {
          try await self.player.play()
        }
      } else {
        await attempt(on: entry, epoch: epoch, deadline: deadline) {
          try await self.player.prepareToPlay()
        }
      }
      guard epoch == self.epoch else { return }
    }
    player.playbackTime = seconds
    furthest = seconds
    stalledSince = nil
    // Playback that has not begun yet may drop the seek, so a start in this batch repeats it.
    pendingOffset = isPlaying ? nil : seconds
  }

  private func start(epoch: Int, deadline: ContinuousClock.Instant) async {
    wantPlaying = true
    // An entry that played out is not started again: MusicKit, or the host, moves on from it
    // and plays on.
    guard !ended, stalledSince == nil, !advancing, let entry = currentEntry, entry.queued,
      !entry.failed
    else { return }
    if !isPlaying {
      sawPlaying = false
      startedAt = .now
    }
    await attempt(on: entry, epoch: epoch, deadline: deadline) { try await self.player.play() }
    if let offset = pendingOffset, epoch == self.epoch {
      player.playbackTime = offset
      furthest = offset
      pendingOffset = nil
    }
  }

  private func stop() {
    wantPlaying = false
    stoppedAt = .now
    startedAt = nil
    player.pause()
  }

  /// Makes the entry at `index` current, from `offset`.
  private func becomeCurrent(_ index: Int, at offset: Double = 0) {
    current = index
    sawPlaying = false
    furthest = offset
    stalledSince = nil
    advancing = false
    startedAt = wantPlaying ? .now : nil
  }

  /// Gives MusicKit a fresh queue that starts with `entry`. That pauses what plays, so it is only
  /// done when nothing does. The entries after it follow by inserts, as found songs do.
  private func replaceQueue(startingWith entry: Entry, _ song: Song) {
    player.queue = ApplicationMusicPlayer.Queue(for: [song])
    observeQueue()
    for other in entries {
      other.queued = false
      other.queueEntryID = nil
    }
    entry.queued = true
    entry.queueEntryID = player.queue.currentEntry?.id ?? player.queue.entries.first?.id
    if entry.queueEntryID == nil {
      NSLog("%@", "Apple Music host: a new queue has no entry id yet; its song will tell")
    }
    let epoch = epoch
    enqueue { [weak self] in await self?.insertFound(epoch: epoch) }
  }

  // MARK: - Entries

  /// Looks up an entry's song in the background, and inserts it into MusicKit's queue in line
  /// with the batches once found.
  private func lookUp(_ entry: Entry, epoch: Int) {
    guard entry.song == nil, entry.lookup == nil, !entry.failed else { return }
    entry.lookup = Task { [weak self] in
      guard let self else { return }
      let found = await self.resolve(entry, epoch: epoch, deadline: .now + Self.timeout)
      guard found != nil, epoch == self.epoch, self.index(of: entry) != nil else { return }
      self.enqueue { [weak self] in await self?.insertFound(epoch: epoch) }
    }
  }

  /// The song of an entry, waiting for its lookup until `deadline`. An entry whose song is not
  /// found by then fails.
  private func song(for entry: Entry, epoch: Int, deadline: ContinuousClock.Instant) async
    -> Song?
  {
    if let song = entry.song { return song }
    guard !entry.failed else { return nil }
    guard let lookup = entry.lookup else {
      return await resolve(entry, epoch: epoch, deadline: deadline)
    }
    _ = try? await bounded(until: deadline) { await lookup.value }
    guard epoch == self.epoch, index(of: entry) != nil else { return nil }
    if entry.song == nil, !entry.failed {
      lookup.cancel()
      fail(entry, .network)
    }
    return entry.song
  }

  /// Puts the songs found for the entries after the current one into MusicKit's queue, in order,
  /// up to the first one that is not found yet or failed, so that MusicKit never moves on to an
  /// entry that cannot be played.
  private func insertFound(epoch: Int) async {
    guard epoch == self.epoch, !ended, let current, let playing = currentEntry, playing.queued,
      !playing.failed
    else { return }
    for entry in entries[(current + 1)...] {
      if entry.queued { continue }
      guard !entry.failed, let song = entry.song else { return }
      await insert(entry, song, epoch: epoch, deadline: .now + Self.insertTimeout)
      guard epoch == self.epoch, entry.queued else { return }
    }
  }

  /// Inserts an entry's song at the tail of MusicKit's queue, which leaves what plays alone, and
  /// remembers which MusicKit entry it became.
  private func insert(
    _ entry: Entry, _ song: Song, epoch: Int, deadline: ContinuousClock.Instant
  ) async {
    let before = Set(player.queue.entries.map(\.id))
    let work = Task { try await self.player.queue.insert([song], position: .tail) }
    do {
      try await bounded(until: deadline) { try await work.value }
    } catch {
      abandon(work)
      if Self.isCancellation(error) || epoch != self.epoch || index(of: entry) == nil { return }
      NSLog("%@", "Apple Music host: queueing a song failed: \(error)")
      failure(of: error, entry)
      return
    }
    guard epoch == self.epoch, index(of: entry) != nil else {
      pruneQueue()
      return
    }
    entry.queued = true
    entry.queueEntryID = player.queue.entries.last { !before.contains($0.id) }?.id
    if entry.queueEntryID == nil {
      NSLog("%@", "Apple Music host: an inserted song has no new entry id; its song will tell")
    }
  }

  /// An insert that was given up on may still land, and MusicKit would then hold a song that no
  /// entry maps to. What it added is taken out at once, and again once it is done.
  private func abandon(_ work: Task<Void, Error>) {
    work.cancel()
    pruneQueue()
    Task { [weak self] in
      _ = try? await work.value
      self?.enqueue { [weak self] in self?.pruneQueue() }
    }
  }

  /// Takes the entries after the one that plays out of MusicKit's queue when no entry of the host
  /// maps to them: an insert that landed after it was given up on, or an entry that left or
  /// failed. MusicKit never moves on to a song the player did not give or took back.
  private func pruneQueue() {
    let queue = player.queue.entries
    var start = 0
    if let playing = player.queue.currentEntry?.id,
      let position = queue.firstIndex(where: { $0.id == playing })
    {
      start = position + 1
    }
    let mapped = entries.filter { $0.queued && !$0.failed }
    for position in stride(from: queue.count - 1, through: start, by: -1) {
      let queued = queue[position]
      if mapped.contains(where: { plays(queued, $0) }) { continue }
      NSLog("%@", "Apple Music host: taking a queue entry that maps to no track out of the queue")
      player.queue.entries.remove(at: position)
    }
  }

  /// The current entry played out while an entry follows it, and MusicKit did not move on: the
  /// next song came too late for its queue, or is still being looked up. The host moves on
  /// itself, or ends when the next entry cannot be played.
  private func advanceByHand(to next: Entry) {
    advancing = true
    let epoch = epoch
    Task { [weak self] in
      guard let self else { return }
      // The lookup is waited for outside the line of batches, so that a pause still goes
      // through meanwhile, and no longer than a batch would. The move itself goes in line.
      _ = await self.song(for: next, epoch: epoch, deadline: .now + Self.batchBudget)
      guard epoch == self.epoch else { return }
      self.enqueue { [weak self] in
        guard let self, epoch == self.epoch else { return }
        defer { self.advancing = false }
        await self.moveOn(to: next, epoch: epoch)
      }
    }
  }

  private func moveOn(to next: Entry, epoch: Int) async {
    followMusicKit()
    // MusicKit moved on by itself, the entry left, or a seek took the current entry back.
    guard !ended, stalledSince != nil, let current, index(of: next) == current + 1 else {
      return
    }
    guard let song = next.song, !next.failed else {
      finish()
      return
    }
    // Nothing plays any more, so a fresh queue costs no gap.
    replaceQueue(startingWith: next, song)
    becomeCurrent(current + 1)
    sendStatus()
    if wantPlaying {
      await attempt(on: next, epoch: epoch, deadline: .now + Self.batchBudget) {
        try await self.player.play()
      }
    }
  }

  // MARK: - Matching MusicKit's entries

  /// The song a MusicKit queue entry plays.
  private static func songID(of queued: MusicPlayer.Queue.Entry) -> MusicItemID? {
    if case .song(let song)? = queued.item {
      return song.id
    }
    return (queued.transientItem as? Song)?.id
  }

  /// Whether MusicKit's queue holds no entry under the id `entry` knows, so that only its song
  /// can tell which queue entry it is.
  private func lostQueueEntry(_ entry: Entry) -> Bool {
    guard let id = entry.queueEntryID else { return true }
    return !player.queue.entries.contains { $0.id == id }
  }

  /// Whether MusicKit's `queued` plays `entry`: by its id, or else by its song when the id that
  /// `entry` knows is missing or gone, as when MusicKit gave the entry a new one, and no other
  /// entry has that queue entry. A match by the song takes on the new id.
  private func plays(_ queued: MusicPlayer.Queue.Entry, _ entry: Entry) -> Bool {
    guard entry.queued else { return false }
    if queued.id == entry.queueEntryID { return true }
    guard lostQueueEntry(entry), let song = entry.song, Self.songID(of: queued) == song.id,
      !entries.contains(where: { $0 !== entry && $0.queueEntryID == queued.id })
    else { return false }
    NSLog(
      "%@",
      "Apple Music host: matched a queue entry by its song, as its id was "
        + (entry.queueEntryID == nil ? "unknown" : "gone"))
    entry.queueEntryID = queued.id
    return true
  }

  /// Whether MusicKit's current entry plays `entry`.
  private func musicKitPlays(_ entry: Entry) -> Bool {
    guard let queued = player.queue.currentEntry else { return false }
    return plays(queued, entry)
  }

  // MARK: - MusicKit calls

  /// Runs `work` until `deadline`, or until the task that waits for it is cancelled, as a newer
  /// pick does. The work keeps running if MusicKit ignores the cancel, but nobody waits for it
  /// any more.
  private func bounded<T: Sendable>(
    until deadline: ContinuousClock.Instant,
    _ work: @escaping @MainActor @Sendable () async throws -> T
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
      do {
        try await Task.sleep(until: deadline, clock: .continuous)
      } catch {
        return
      }
      gate.resolve(.failure(TimedOut()))
      worker.cancel()
    }
    defer { watchdog.cancel() }
    return try await withTaskCancellationHandler {
      try await gate.wait()
    } onCancel: {
      gate.resolve(.failure(CancellationError()))
      worker.cancel()
    }
  }

  /// Runs one player call for `entry`. A failure is reported unless it only means that a newer
  /// pick took over, or `done` says that what the call was for happened anyway.
  private func attempt(
    on entry: Entry, epoch: Int, deadline: ContinuousClock.Instant,
    unless done: @autoclosure () -> Bool = false,
    _ work: @escaping @MainActor @Sendable () async throws -> Void
  ) async {
    do {
      try await bounded(until: deadline, work)
    } catch {
      if Self.isCancellation(error) || epoch != self.epoch || done() { return }
      NSLog("%@", "Apple Music host: playback failed: \(error)")
      failure(of: error, entry)
    }
  }

  /// A queue that replaces one that is still starting interrupts its `play()`; that is routine.
  private static func isCancellation(_ error: Error) -> Bool {
    if error is CancellationError { return true }
    let nsError = error as NSError
    return nsError.domain == "MPMusicPlayerControllerErrorDomain" && nsError.code == 2
  }

  /// Reports a failed MusicKit call. Unplayable tells the player that the track is gone from
  /// the catalogue, so only what says so maps to it. `play()` surfaces MediaPlayer errors, so
  /// MPErrorDomain is read as well as the URL and MusicKit errors.
  private func failure(of error: Error, _ entry: Entry) {
    if error is TimedOut || error is PlaybackStopped {
      fail(entry, .network)
      return
    }
    if MusicAuthorization.currentStatus != .authorized || error is MusicTokenRequestError {
      fail(entry, .notAuthorized)
      return
    }
    if let request = error as? MusicDataRequest.Error {
      switch request.status {
      case 401, 403: fail(entry, .notAuthorized)
      case 429, 500...599: fail(entry, .network)
      default: fail(entry, .unplayable)
      }
      return
    }
    let nsError = error as NSError
    switch (nsError.domain, nsError.code) {
    case (NSURLErrorDomain, _), ("MPErrorDomain", 3), ("MPErrorDomain", 7):
      fail(entry, .network)
    case ("MPErrorDomain", 1):
      fail(entry, .notAuthorized)
    case ("MPErrorDomain", 2):
      // The subscription lapsed, so nothing can be played any more. The queue goes at once, so
      // the batch stops here, and the host starts again as one that cannot play, once.
      guard !lapseReported else { return }
      lapseReported = true
      drop()
      lapsed()
    default:
      if let sub = error as? MusicSubscription.Error, sub == .permissionDenied {
        fail(entry, .notAuthorized)
      } else if let library = error as? MusicLibrary.Error, library == .permissionDenied {
        fail(entry, .notAuthorized)
      } else {
        fail(entry, .unplayable)
      }
    }
  }

  // MARK: - Songs

  /// The song of an entry. One that cannot be found, or whose lookup fails, fails the entry,
  /// unless the entry left the queue meanwhile.
  private func resolve(_ entry: Entry, epoch: Int, deadline: ContinuousClock.Instant) async
    -> Song?
  {
    if let song = entry.song { return song }
    let id = entry.id
    if let cached = songs[id] {
      entry.song = cached
      return cached
    }
    do {
      let found = try await bounded(until: deadline) { try await Self.lookUpSong(id) }
      guard epoch == self.epoch, index(of: entry) != nil else { return nil }
      guard let found else {
        fail(entry, .unplayable)
        return nil
      }
      if songs.count > 2000 { songs.removeAll() }
      songs[id] = found
      entry.song = found
      return found
    } catch {
      if Self.isCancellation(error) || epoch != self.epoch || index(of: entry) == nil {
        return nil
      }
      NSLog("%@", "Apple Music host: looking up a song failed: \(error)")
      failure(of: error, entry)
      return nil
    }
  }

  /// `i.…` is a library song, `catalog:<id>` a catalogue song.
  nonisolated private static func lookUpSong(_ id: String) async throws -> Song? {
    if let catalogID = id.removingPrefix("catalog:") {
      let request = MusicCatalogResourceRequest<Song>(
        matching: \.id, equalTo: MusicItemID(catalogID))
      return try await request.response().items.first
    }
    var request = MusicLibraryRequest<Song>()
    request.filter(matching: \.id, equalTo: MusicItemID(id))
    return try await request.response().items.first
  }

  // MARK: - Events

  /// Reports that an entry cannot be played, once. MusicKit never moves on to it.
  private func fail(_ entry: Entry, _ reason: Failure) {
    guard !entry.failed, let index = index(of: entry) else { return }
    entry.failure = reason
    entry.lookup?.cancel()
    sendFailed(index, reason)
    if entry.queued, index != current {
      pruneQueue()
    }
  }

  private func sendFailed(_ index: Int, _ reason: Failure) {
    send(
      encode([
        "type": "jukeboxItemFailed",
        "host": "appleMusic",
        "revision": revision,
        "index": index,
        "reason": reason.rawValue,
      ]))
  }

  private func sendEnded() {
    send(encode(["type": "jukeboxEnded", "host": "appleMusic", "revision": revision]))
  }

  /// Whether the current entry plays, as Rune should see it. One that starts, or that moves on
  /// to the next entry, still plays.
  private var reportsPlaying: Bool {
    guard !ended, let entry = currentEntry, entry.queued, !entry.failed, wantPlaying else {
      return false
    }
    return isPlaying || !sawPlaying || stalledSince != nil || advancing
  }

  private var position: Double {
    guard let entry = currentEntry, entry.queued, !entry.failed else { return 0 }
    let time = max(0, player.playbackTime)
    return ended || stalledSince != nil ? max(time, furthest) : time
  }

  private func sendStatus() {
    reportedSignature = signature
    reportedAt = .now
    send(
      encode([
        "type": "jukeboxStatus",
        "host": "appleMusic",
        "revision": revision,
        "currentIndex": current.map { $0 as Any } ?? NSNull(),
        "currentId": currentEntry.map { $0.id as Any } ?? NSNull(),
        "playing": reportsPlaying,
        "positionMs": Int((position * 1000).rounded()),
      ]))
  }

  /// What Rune can see of the player, so a change it did not cause is noticed.
  private var signature: String {
    "\(String(describing: current))|\(ended)|\(reportsPlaying)|\(currentEntry?.failed ?? false)"
  }

  /// Notices the host moving on, ending or failing, and reports a change that Rune did not
  /// cause. Nothing is sent while a batch is applied: the batch answers once it is done.
  private func check(periodic: Bool) {
    guard !applying else { return }
    reconcile()
    guard current != nil else { return }
    let due = periodic && reportsPlaying && ContinuousClock.now - reportedAt >= Self.reportEvery
    if signature != reportedSignature || due {
      sendStatus()
    }
  }

  /// Follows MusicKit when its current entry changed to the next entry's: the host moved on by
  /// itself, at the end of an entry or through the system's media controls.
  private func followMusicKit() {
    guard !ended, let current, let entry = currentEntry, entry.queued,
      let queued = player.queue.currentEntry
    else { return }
    if queued.id == entry.queueEntryID { return }
    let next = nextEntry
    if let next, next.queued, queued.id == next.queueEntryID {
      becomeCurrent(current + 1)
      return
    }
    // Without a match by id the song tells, the current entry's first, so that the same song
    // twice is never taken for a move.
    if plays(queued, entry) { return }
    if let next, plays(queued, next) {
      becomeCurrent(current + 1)
      return
    }
    if lostQueueEntry(entry) {
      // Nothing MusicKit was given plays before the current entry.
      NSLog("%@", "Apple Music host: matched the playing queue entry by its place, not id or song")
      entry.queueEntryID = queued.id
      return
    }
    if unmatchedEntryID != queued.id {
      unmatchedEntryID = queued.id
      NSLog("%@", "Apple Music host: MusicKit plays neither the current nor the next track's entry")
    }
  }

  private func reconcile() {
    followMusicKit()
    guard !applying, !advancing, !ended, let entry = currentEntry, !entry.failed, entry.queued
    else { return }
    let now = ContinuousClock.now
    let time = max(0, player.playbackTime)
    if isPlaying {
      // A resume from outside Rune, such as the system's media controls.
      if !wantPlaying, now - stoppedAt > .seconds(1) {
        wantPlaying = true
      }
      sawPlaying = true
      startedAt = nil
      furthest = max(furthest, time)
      stalledSince = nil
      return
    }
    if wantPlaying, !sawPlaying, let startedAt, now - startedAt >= Self.startLimit {
      // Told to play, but never heard playing.
      self.startedAt = nil
      NSLog("%@", "Apple Music host: playback did not start in time")
      fail(entry, .network)
      return
    }
    // MusicKit plays something else, or nothing was heard yet.
    let playing = player.queue.currentEntry?.id
    guard sawPlaying, playing == nil || playing == entry.queueEntryID else { return }
    let status = player.state.playbackStatus
    let reached = max(time, furthest)
    let playedOut: Bool
    if let duration = entry.song?.duration, duration > 0 {
      // Playing out ends the entry whether MusicKit then shows stopped or paused.
      playedOut = reached >= duration - 1
    } else {
      playedOut = status == .stopped
    }
    guard playedOut else {
      if status == .stopped {
        // Stopped well before the end, with no stop or clear from Rune.
        failure(of: PlaybackStopped(), entry)
      }
      return
    }
    guard let next = nextEntry, !next.failed else {
      finish()
      return
    }
    // MusicKit moves on to an entry in its queue by itself; give it a moment before helping.
    guard let since = stalledSince else {
      stalledSince = now
      return
    }
    if now - since >= .seconds(1) {
      advanceByHand(to: next)
    }
  }

  /// The last entry finished, and nothing playable follows it.
  private func finish() {
    ended = true
    stalledSince = nil
    sendEnded()
    sendStatus()
  }
}

extension String {
  nonisolated fileprivate func removingPrefix(_ prefix: String) -> String? {
    hasPrefix(prefix) ? String(dropFirst(prefix.count)) : nil
  }
}
