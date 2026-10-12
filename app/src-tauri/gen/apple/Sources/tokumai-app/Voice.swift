// Voice.swift — speech to text that never leaves the phone.
//
// A question can be spoken instead of typed. The microphone feeds Apple's on-device
// speech recognition and only the text goes on — exactly as if it had been typed. There
// is no recording: audio buffers are handed to the recogniser as they arrive and nothing
// is written anywhere.
//
// Two engines, by iOS version:
//   · iOS 26+: `SpeechAnalyzer` + `SpeechTranscriber`, which runs on the device by
//     construction and fetches its language model itself (`AssetInventory`) — the first
//     use may download the model from Apple; the audio never goes anywhere.
//   · earlier: `SFSpeechRecognizer` with `requiresOnDeviceRecognition = true`, which
//     needs the dictation language installed (Settings → General → Keyboard → Dictation)
//     and fails with "failed to access assets" otherwise — said in plain words.
//
// Five C-callable functions, the shape of StoreKitShim.swift: Rust (src-tauri/src/voice.rs)
// calls, Swift answers once through the callback with a JSON string. `poll` is called a
// dozen times a second while listening and answers with the state, the text so far and
// the input level for the waveform. States: idle, installing (model download, with
// `progress`), listening, stopping, final, error.

import AVFoundation
import Foundation
import Speech

public typealias TokumaiVoiceCallback = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void

private struct VoiceReply: @unchecked Sendable {
    let ctx: UnsafeMutableRawPointer?
    let cb: TokumaiVoiceCallback
    func send(_ obj: [String: Any]) {
        let json: String
        if let d = try? JSONSerialization.data(withJSONObject: obj), let s = String(data: d, encoding: .utf8) {
            json = s
        } else {
            json = "{\"error\":\"could not encode the reply\"}"
        }
        json.withCString { cb(ctx, $0) }
    }
    func fail(_ message: String) { send(["error": message]) }
}

private func voiceString(_ p: UnsafePointer<CChar>?) -> String {
    guard let p = p else { return "" }
    return String(cString: p)
}

/// The locale to listen in: the one asked for, else the phone's own.
private func voiceLocale(_ lang: String) -> Locale {
    lang.isEmpty ? Locale.current : Locale(identifier: lang)
}

/// "en-US" for a locale, on every iOS this app runs on.
private func bcp47(_ l: Locale) -> String {
    if #available(iOS 16.0, *) { return l.identifier(.bcp47) }
    return l.identifier.replacingOccurrences(of: "_", with: "-")
}

private let noModelHint = "no on-device speech model for this language — your voice would have to leave the phone, so tokumai does not listen. iOS downloads offline languages under Settings → General → Keyboard → Dictation"

/// One listening session at a time. Everything the recogniser and the audio engine report
/// lands here under the lock; `poll` reads a snapshot.
private final class VoiceSession {
    static let shared = VoiceSession()

    private let lock = NSLock()
    private var state = "idle"
    private var text = ""
    private var error = ""
    private var level: Float = 0
    private var progress: Double = 0

    // The microphone, shared by both engines.
    private var engine: AVAudioEngine?

    // iOS 15–25: SFSpeechRecognizer.
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private var recognizer: SFSpeechRecognizer?

    // iOS 26+: SpeechAnalyzer. Kept as Any so the class loads on older systems.
    private var modern: Any?

    func snapshot() -> [String: Any] {
        lock.lock(); defer { lock.unlock() }
        var out: [String: Any] = ["state": state, "text": text, "level": level]
        if !error.isEmpty { out["error"] = error }
        if state == "installing" { out["progress"] = progress }
        return out
    }

    fileprivate func set(_ f: () -> Void) {
        lock.lock(); f(); lock.unlock()
    }
    fileprivate func setText(_ t: String) { set { self.text = t } }
    fileprivate func setProgress(_ p: Double) { set { self.progress = p } }
    fileprivate func setState(_ s: String) { set { self.state = s } }
    fileprivate func fail(_ why: String) { set { self.state = "error"; self.error = why } }
    fileprivate func currentState() -> String { lock.lock(); defer { lock.unlock() }; return state }

    /// Does this phone know the language at all? Said before the button is shown. Whether
    /// the model is on the phone is settled at `start`: the new engine fetches it, the old
    /// one says what is missing.
    func available(lang: String, reply: VoiceReply) {
        guard SFSpeechRecognizer(locale: voiceLocale(lang)) != nil else {
            reply.send(["available": false, "why": "speech recognition does not know this language"])
            return
        }
        // The languages on offer for the "Speak in" setting, named in the phone's language.
        let name = { (l: Locale) -> [String: Any] in
            ["tag": bcp47(l), "name": Locale.current.localizedString(forIdentifier: l.identifier) ?? l.identifier]
        }
        if #available(iOS 26.0, *) {
            Task {
                let list = await SpeechTranscriber.supportedLocales.map(name).sorted { ($0["name"] as? String ?? "") < ($1["name"] as? String ?? "") }
                reply.send(["available": true, "lang": bcp47(voiceLocale(lang)), "languages": list])
            }
        } else {
            let list = SFSpeechRecognizer.supportedLocales().map(name).sorted { ($0["name"] as? String ?? "") < ($1["name"] as? String ?? "") }
            reply.send(["available": true, "lang": bcp47(voiceLocale(lang)), "languages": list])
        }
    }

    func start(lang: String, reply: VoiceReply) {
        lock.lock()
        if state == "listening" || state == "stopping" || state == "installing" {
            lock.unlock()
            reply.fail("already listening")
            return
        }
        state = "idle"; text = ""; error = ""; level = 0; progress = 0
        lock.unlock()

        let begin = {
            AVAudioSession.sharedInstance().requestRecordPermission { ok in
                guard ok else {
                    reply.fail("the microphone is not allowed for tokumai — Settings → tokumai → Microphone")
                    return
                }
                DispatchQueue.main.async {
                    if #available(iOS 26.0, *) {
                        self.beginModern(lang: lang, reply: reply)
                    } else {
                        self.beginLegacy(lang: lang, reply: reply)
                    }
                }
            }
        }
        if #available(iOS 26.0, *) {
            // SpeechAnalyzer needs no speech-recognition authorisation, only the microphone.
            begin()
        } else {
            SFSpeechRecognizer.requestAuthorization { auth in
                guard auth == .authorized else {
                    reply.fail("speech recognition is not allowed for tokumai — Settings → tokumai → Speech Recognition")
                    return
                }
                begin()
            }
        }
    }

    /// The microphone, open and measured. The tap hands every buffer to `sink`.
    private func openMicrophone(sink: @escaping (AVAudioPCMBuffer) -> Void) throws -> AVAudioEngine {
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(.record, mode: .measurement, options: .duckOthers)
        try session.setActive(true, options: .notifyOthersOnDeactivation)
        let engine = AVAudioEngine()
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
            self?.measure(buffer)
            sink(buffer)
        }
        engine.prepare()
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            try? session.setActive(false, options: .notifyOthersOnDeactivation)
            throw error
        }
        return engine
    }

    // ---- iOS 26+: SpeechAnalyzer ----------------------------------------------------------

    @available(iOS 26.0, *)
    private func beginModern(lang: String, reply: VoiceReply) {
        let m = ModernEngine(session: self, locale: voiceLocale(lang))
        set { self.modern = m }
        Task { await m.run(reply: reply) }
    }

    // ---- iOS 15–25: SFSpeechRecognizer -----------------------------------------------------

    private func beginLegacy(lang: String, reply: VoiceReply) {
        guard let recognizer = SFSpeechRecognizer(locale: voiceLocale(lang)), recognizer.isAvailable else {
            reply.fail("speech recognition is not available right now")
            return
        }
        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        // The promise. Without it the recogniser may fall back to Apple's servers.
        request.requiresOnDeviceRecognition = true
        if #available(iOS 16.0, *) {
            request.addsPunctuation = true
        }
        let engine: AVAudioEngine
        do {
            engine = try openMicrophone { buffer in request.append(buffer) }
        } catch {
            reply.fail("the microphone could not be opened: \(error.localizedDescription)")
            return
        }
        set {
            self.engine = engine
            self.request = request
            self.recognizer = recognizer
            self.state = "listening"
        }
        self.task = recognizer.recognitionTask(with: request) { [weak self] result, err in
            guard let self = self else { return }
            self.lock.lock()
            if let r = result {
                self.text = r.bestTranscription.formattedString
                if r.isFinal {
                    self.state = "final"
                }
            }
            if let e = err, self.state != "final" {
                // After `stop`, "no speech detected" is simply an empty answer; while
                // listening it is a fault worth saying.
                if self.state == "stopping" || !self.text.isEmpty {
                    self.state = "final"
                } else if self.state == "listening" {
                    self.state = "error"
                    let why = e.localizedDescription
                    let code = (e as NSError).code
                    self.error = why.lowercased().contains("asset") || why.lowercased().contains("on-device") || code == 1101 || code == 1107
                        ? noModelHint
                        : "could not listen: \(why)"
                }
            }
            let done = self.state == "final" || self.state == "error"
            self.lock.unlock()
            if done { self.teardown() }
        }
        reply.send(["ok": true, "lang": voiceLocale(lang).identifier, "engine": "legacy"])
    }

    /// Input level 0…1 from the buffer's RMS, for the waveform. Speech sits around
    /// 0.02–0.2 RMS; ×6 puts a normal voice in the upper half of the bar.
    private func measure(_ buffer: AVAudioPCMBuffer) {
        guard let data = buffer.floatChannelData?[0] else { return }
        let n = Int(buffer.frameLength)
        if n == 0 { return }
        var sum: Float = 0
        for i in 0..<n { sum += data[i] * data[i] }
        let rms = (sum / Float(n)).squareRoot()
        set { self.level = min(1, rms * 6) }
    }

    /// The person is done talking: close the microphone, let the recogniser finish. The
    /// final words arrive through the engine's own callback.
    func stop() {
        lock.lock()
        guard state == "listening" else { lock.unlock(); return }
        state = "stopping"
        let engine = self.engine
        let request = self.request
        let modern = self.modern
        lock.unlock()
        engine?.stop()
        engine?.inputNode.removeTap(onBus: 0)
        request?.endAudio()
        if #available(iOS 26.0, *), let m = modern as? ModernEngine {
            Task { await m.finish() }
        }
    }

    func cancel() {
        lock.lock()
        let task = self.task
        let modern = self.modern
        state = "idle"; text = ""; error = ""
        lock.unlock()
        task?.cancel()
        if #available(iOS 26.0, *), let m = modern as? ModernEngine {
            m.cancel()
        }
        teardown()
    }

    fileprivate func adopt(engine: AVAudioEngine) {
        set { self.engine = engine }
    }

    fileprivate func teardown() {
        lock.lock()
        let engine = self.engine
        self.engine = nil
        self.request = nil
        self.task = nil
        self.recognizer = nil
        self.modern = nil
        level = 0
        lock.unlock()
        engine?.stop()
        engine?.inputNode.removeTap(onBus: 0)
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    fileprivate func openMicrophoneForModern(sink: @escaping (AVAudioPCMBuffer) -> Void) throws -> AVAudioEngine {
        try openMicrophone(sink: sink)
    }
}

/// iOS 26's analyzer: on the device by construction. Volatile results replace each other
/// inside the current phrase; final results are kept in order. The model is fetched on
/// first use (state "installing", with progress) — a download from Apple of the model,
/// never an upload of the voice.
@available(iOS 26.0, *)
private final class ModernEngine {
    private unowned let session: VoiceSession
    private let locale: Locale
    private var analyzer: SpeechAnalyzer?
    private var transcriber: SpeechTranscriber?
    private var continuation: AsyncStream<AnalyzerInput>.Continuation?
    private var converter: AVAudioConverter?
    private var results: Task<Void, Never>?
    private var finished = ""
    private var volatile = ""
    private let lock = NSLock()

    init(session: VoiceSession, locale: Locale) {
        self.session = session
        self.locale = locale
    }

    /// The model's locale for the phone's: the same one if Apple has it, else the same
    /// LANGUAGE — a phone set to English in Germany says "en_DE", and the models are
    /// en_US, en_GB, …; en_US is the one to take for English with no region of its own.
    private static func pick(_ wanted: Locale, from supported: [Locale]) -> Locale? {
        let want = wanted.identifier(.bcp47)
        if let exact = supported.first(where: { $0.identifier(.bcp47) == want }) { return exact }
        let lang = wanted.language.languageCode?.identifier ?? ""
        let same = supported.filter { $0.language.languageCode?.identifier == lang }
        return same.first(where: { $0.identifier(.bcp47).hasSuffix("-US") })
            ?? same.first(where: { $0.identifier(.bcp47).hasSuffix("-\(lang.uppercased())") })
            ?? same.first
    }

    func run(reply: VoiceReply) async {
        // The language, and its model on the phone.
        let supported = await SpeechTranscriber.supportedLocales
        guard let modelLocale = Self.pick(locale, from: supported) else {
            reply.fail("no on-device speech model for \(locale.identifier) — your voice would have to leave the phone, so tokumai does not listen")
            return
        }
        let transcriber = SpeechTranscriber(locale: modelLocale, transcriptionOptions: [], reportingOptions: [.volatileResults], attributeOptions: [])
        do {
            if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
                session.setState("installing")
                let progress = request.progress
                let watch = Task { [weak self] in
                    while !Task.isCancelled {
                        self?.session.setProgress(progress.fractionCompleted)
                        try? await Task.sleep(nanoseconds: 200_000_000)
                    }
                }
                defer { watch.cancel() }
                try await request.downloadAndInstall()
            }
        } catch {
            session.fail("the speech model could not be fetched: \(error.localizedDescription)")
            reply.fail("the speech model could not be fetched: \(error.localizedDescription)")
            return
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            session.fail("the speech analyzer has no audio format")
            reply.fail("the speech analyzer has no audio format")
            return
        }
        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
        lock.lock()
        self.analyzer = analyzer
        self.transcriber = transcriber
        self.continuation = continuation
        lock.unlock()

        results = Task { [weak self] in
            do {
                for try await r in transcriber.results {
                    guard let self = self else { return }
                    let t = String(r.text.characters)
                    self.lock.lock()
                    if r.isFinal {
                        self.finished = self.finished.isEmpty ? t : self.finished + " " + t
                        self.volatile = ""
                    } else {
                        self.volatile = t
                    }
                    let shown = self.volatile.isEmpty ? self.finished : (self.finished.isEmpty ? self.volatile : self.finished + " " + self.volatile)
                    self.lock.unlock()
                    self.session.setText(shown)
                }
                // The stream ended: everything is final.
                guard let self = self else { return }
                self.session.setText(self.finished)
                if self.session.currentState() != "idle" { self.session.setState("final") }
                self.session.teardown()
            } catch {
                guard let self = self else { return }
                if self.session.currentState() == "stopping" || !self.finished.isEmpty {
                    self.session.setText(self.finished)
                    self.session.setState("final")
                } else {
                    self.session.fail("could not listen: \(error.localizedDescription)")
                }
                self.session.teardown()
            }
        }
        do {
            try await analyzer.start(inputSequence: stream)
        } catch {
            session.fail("the speech analyzer could not start: \(error.localizedDescription)")
            reply.fail("the speech analyzer could not start: \(error.localizedDescription)")
            return
        }
        let engine: AVAudioEngine
        do {
            engine = try await MainActor.run {
                try session.openMicrophoneForModern { [weak self] buffer in
                    guard let self = self, let converted = try? self.convert(buffer, to: format) else { return }
                    self.continuation?.yield(AnalyzerInput(buffer: converted))
                }
            }
        } catch {
            continuation.finish()
            session.fail("the microphone could not be opened: \(error.localizedDescription)")
            reply.fail("the microphone could not be opened: \(error.localizedDescription)")
            return
        }
        session.adopt(engine: engine)
        session.setState("listening")
        reply.send(["ok": true, "lang": modelLocale.identifier, "engine": "analyzer"])
    }

    /// The microphone's format into the analyzer's.
    private func convert(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) throws -> AVAudioPCMBuffer {
        if buffer.format == format { return buffer }
        if converter == nil || converter?.outputFormat != format {
            converter = AVAudioConverter(from: buffer.format, to: format)
        }
        guard let converter = converter else { throw NSError(domain: "tokumai.voice", code: 1) }
        let ratio = converter.outputFormat.sampleRate / converter.inputFormat.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up))
        guard let out = AVAudioPCMBuffer(pcmFormat: converter.outputFormat, frameCapacity: capacity) else { throw NSError(domain: "tokumai.voice", code: 2) }
        var err: NSError?
        var handed = false
        let status = converter.convert(to: out, error: &err) { _, inputStatus in
            defer { handed = true }
            inputStatus.pointee = handed ? .noDataNow : .haveData
            return handed ? nil : buffer
        }
        if status == .error { throw err ?? NSError(domain: "tokumai.voice", code: 3) }
        return out
    }

    /// No more audio: the analyzer finishes what it has, the results task sees the end.
    func finish() async {
        lock.lock()
        let continuation = self.continuation
        let analyzer = self.analyzer
        lock.unlock()
        continuation?.finish()
        try? await analyzer?.finalizeAndFinishThroughEndOfInput()
    }

    func cancel() {
        lock.lock()
        let continuation = self.continuation
        let results = self.results
        let analyzer = self.analyzer
        lock.unlock()
        continuation?.finish()
        results?.cancel()
        Task { await analyzer?.cancelAndFinishNow() }
    }
}

@_cdecl("tokumai_voice_available")
public func tokumai_voice_available(_ lang: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?, _ cb: @escaping TokumaiVoiceCallback) {
    VoiceSession.shared.available(lang: voiceString(lang), reply: VoiceReply(ctx: ctx, cb: cb))
}

@_cdecl("tokumai_voice_start")
public func tokumai_voice_start(_ lang: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?, _ cb: @escaping TokumaiVoiceCallback) {
    VoiceSession.shared.start(lang: voiceString(lang), reply: VoiceReply(ctx: ctx, cb: cb))
}

@_cdecl("tokumai_voice_poll")
public func tokumai_voice_poll(_ ctx: UnsafeMutableRawPointer?, _ cb: @escaping TokumaiVoiceCallback) {
    VoiceReply(ctx: ctx, cb: cb).send(VoiceSession.shared.snapshot())
}

@_cdecl("tokumai_voice_stop")
public func tokumai_voice_stop(_ ctx: UnsafeMutableRawPointer?, _ cb: @escaping TokumaiVoiceCallback) {
    VoiceSession.shared.stop()
    VoiceReply(ctx: ctx, cb: cb).send(["ok": true])
}

@_cdecl("tokumai_voice_cancel")
public func tokumai_voice_cancel(_ ctx: UnsafeMutableRawPointer?, _ cb: @escaping TokumaiVoiceCallback) {
    VoiceSession.shared.cancel()
    VoiceReply(ctx: ctx, cb: cb).send(["ok": true])
}
