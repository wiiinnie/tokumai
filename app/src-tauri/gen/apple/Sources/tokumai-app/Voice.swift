// Voice.swift — speech to text that never leaves the phone.
//
// A question can be spoken instead of typed. The microphone feeds Apple's speech
// recogniser with `requiresOnDeviceRecognition = true`: the audio is transcribed by the
// phone itself, and only the text goes on — exactly as if it had been typed. If the
// phone has no on-device model for the language, tokumai does not listen at all rather
// than let the voice go to a server. There is no recording: buffers are appended to the
// recogniser as they arrive and nothing is written anywhere.
//
// Five C-callable functions, the shape of StoreKitShim.swift: Rust (src-tauri/src/voice.rs)
// calls, Swift answers once through the callback with a JSON string. `poll` is called a
// dozen times a second while listening and answers with the state, the text so far and
// the input level for the waveform.

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

/// One listening session at a time. Everything the recogniser and the audio engine report
/// lands here under the lock; `poll` reads a snapshot.
private final class VoiceSession {
    static let shared = VoiceSession()

    private let lock = NSLock()
    /// idle | listening | stopping | final | error
    private var state = "idle"
    private var text = ""
    private var error = ""
    private var level: Float = 0

    private var engine: AVAudioEngine?
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private var recognizer: SFSpeechRecognizer?

    func snapshot() -> [String: Any] {
        lock.lock(); defer { lock.unlock() }
        var out: [String: Any] = ["state": state, "text": text, "level": level]
        if !error.isEmpty { out["error"] = error }
        return out
    }

    /// Can this phone transcribe the language on the device? Said before the button is
    /// shown, so nobody taps a microphone that would have to send their voice away.
    func available(lang: String) -> [String: Any] {
        guard let r = SFSpeechRecognizer(locale: voiceLocale(lang)) else {
            return ["available": false, "why": "speech recognition does not know this language"]
        }
        if !r.supportsOnDeviceRecognition {
            return ["available": false, "why": "no on-device speech model for \(voiceLocale(lang).identifier) — your voice would have to leave the phone, so tokumai does not listen"]
        }
        return ["available": true, "lang": voiceLocale(lang).identifier]
    }

    func start(lang: String, reply: VoiceReply) {
        lock.lock()
        if state == "listening" || state == "stopping" {
            lock.unlock()
            reply.fail("already listening")
            return
        }
        state = "idle"; text = ""; error = ""; level = 0
        lock.unlock()

        SFSpeechRecognizer.requestAuthorization { auth in
            guard auth == .authorized else {
                reply.fail("speech recognition is not allowed for tokumai — Settings → tokumai → Speech Recognition")
                return
            }
            AVAudioSession.sharedInstance().requestRecordPermission { ok in
                guard ok else {
                    reply.fail("the microphone is not allowed for tokumai — Settings → tokumai → Microphone")
                    return
                }
                DispatchQueue.main.async { self.begin(lang: lang, reply: reply) }
            }
        }
    }

    private func begin(lang: String, reply: VoiceReply) {
        guard let recognizer = SFSpeechRecognizer(locale: voiceLocale(lang)), recognizer.isAvailable else {
            reply.fail("speech recognition is not available right now")
            return
        }
        guard recognizer.supportsOnDeviceRecognition else {
            reply.fail("no on-device speech model for this language — your voice would have to leave the phone, so tokumai does not listen")
            return
        }
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.record, mode: .measurement, options: .duckOthers)
            try session.setActive(true, options: .notifyOthersOnDeactivation)
        } catch {
            reply.fail("the microphone could not be opened: \(error.localizedDescription)")
            return
        }
        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        // The promise. Without it the recogniser may fall back to Apple's servers.
        request.requiresOnDeviceRecognition = true
        if #available(iOS 16.0, *) {
            request.addsPunctuation = true
        }
        let engine = AVAudioEngine()
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
            request.append(buffer)
            self?.measure(buffer)
        }
        engine.prepare()
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            try? session.setActive(false, options: .notifyOthersOnDeactivation)
            reply.fail("the microphone could not be started: \(error.localizedDescription)")
            return
        }
        lock.lock()
        self.engine = engine
        self.request = request
        self.recognizer = recognizer
        self.state = "listening"
        lock.unlock()

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
                    self.error = e.localizedDescription
                }
            }
            let done = self.state == "final" || self.state == "error"
            self.lock.unlock()
            if done { self.teardown() }
        }
        reply.send(["ok": true, "lang": voiceLocale(lang).identifier])
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
        lock.lock()
        level = min(1, rms * 6)
        lock.unlock()
    }

    /// The person is done talking: close the microphone, let the recogniser finish. The
    /// final result (or "no speech") comes through the task's callback.
    func stop() {
        lock.lock()
        guard state == "listening" else { lock.unlock(); return }
        state = "stopping"
        let engine = self.engine
        let request = self.request
        lock.unlock()
        engine?.stop()
        engine?.inputNode.removeTap(onBus: 0)
        request?.endAudio()
    }

    func cancel() {
        lock.lock()
        let task = self.task
        state = "idle"; text = ""; error = ""
        lock.unlock()
        task?.cancel()
        teardown()
    }

    private func teardown() {
        lock.lock()
        let engine = self.engine
        self.engine = nil
        self.request = nil
        self.task = nil
        self.recognizer = nil
        level = 0
        lock.unlock()
        engine?.stop()
        engine?.inputNode.removeTap(onBus: 0)
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}

@_cdecl("tokumai_voice_available")
public func tokumai_voice_available(_ lang: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?, _ cb: @escaping TokumaiVoiceCallback) {
    VoiceReply(ctx: ctx, cb: cb).send(VoiceSession.shared.available(lang: voiceString(lang)))
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
