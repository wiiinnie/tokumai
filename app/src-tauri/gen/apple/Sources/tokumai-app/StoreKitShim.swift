// StoreKitShim.swift — StoreKit 2 for the Rust core.
//
// StoreKit 2 is Swift-only (async/await), so this file is the whole bridge: four C-callable
// functions the Rust side declares as `extern "C"` (src-tauri/src/iap_ios.rs). Every call
// answers exactly once through the callback with a JSON string; the string is valid only
// during the callback (Rust copies it). Nothing here decides whether credit is granted —
// that is the server's job, on Apple's signed transaction (`jwsRepresentation`), which is
// why a transaction is only finished after the server has said "credited".

import Foundation
import StoreKit

public typealias TokumaiIapCallback = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void

/// The Rust-side reply channel: an opaque pointer and the function to hand it the answer.
private struct Reply: @unchecked Sendable {
    let ctx: UnsafeMutableRawPointer?
    let cb: TokumaiIapCallback
    func send(_ obj: [String: Any]) {
        let json: String
        if let d = try? JSONSerialization.data(withJSONObject: obj), let s = String(data: d, encoding: .utf8) {
            json = s
        } else {
            json = "{\"error\":\"could not encode the App Store reply\"}"
        }
        json.withCString { cb(ctx, $0) }
    }
    func fail(_ message: String) { send(["error": message]) }
}

private func cString(_ p: UnsafePointer<CChar>?) -> String {
    guard let p = p else { return "" }
    return String(cString: p)
}

@available(iOS 15.0, *)
private func describe(_ v: VerificationResult<Transaction>) -> [String: Any]? {
    guard case .verified(let tx) = v else { return nil }
    return ["transactionId": String(tx.id), "productId": tx.productID, "jws": v.jwsRepresentation]
}

/// Products for the given ids (a JSON array of strings), in the order asked. Ids the App
/// Store does not know are left out, so a missing product shows up as a shorter list.
@_cdecl("tokumai_iap_products")
public func tokumai_iap_products(_ idsJson: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?, _ cb: TokumaiIapCallback) {
    let reply = Reply(ctx: ctx, cb: cb)
    let ids: [String] = {
        guard let d = cString(idsJson).data(using: .utf8),
              let a = try? JSONSerialization.jsonObject(with: d) as? [String] else { return [] }
        return a
    }()
    guard #available(iOS 15.0, *) else { reply.fail("iOS 15 or newer is needed for App Store purchases"); return }
    Task {
        do {
            let products = try await Product.products(for: ids)
            let list: [[String: Any]] = ids.compactMap { id in
                guard let p = products.first(where: { $0.id == id }) else { return nil }
                return ["id": p.id, "displayName": p.displayName, "displayPrice": p.displayPrice, "price": "\(p.price)"]
            }
            // StoreKit does not say WHY an identifier came back empty — it simply omits it,
            // and an app that only sees the empty list cannot tell "Apple has not published
            // it" from "we asked for the wrong string". So report what was asked and what
            // was missing; the app shows it in the developer diagnostics.
            let missing = ids.filter { id in !products.contains(where: { $0.id == id }) }
            reply.send(["products": list, "asked": ids, "missing": missing])
        } catch {
            reply.fail("the App Store did not answer: \(error.localizedDescription)")
        }
    }
}

/// Buy one product. `status`: ok (with transactionId, productId, jws) | cancelled | pending
/// (Ask to Buy — the transaction arrives later through `unfinished`).
@_cdecl("tokumai_iap_purchase")
public func tokumai_iap_purchase(_ idC: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?, _ cb: TokumaiIapCallback) {
    let reply = Reply(ctx: ctx, cb: cb)
    let id = cString(idC)
    guard #available(iOS 15.0, *) else { reply.fail("iOS 15 or newer is needed for App Store purchases"); return }
    Task { @MainActor in
        do {
            guard let product = try await Product.products(for: [id]).first else {
                reply.fail("this product is not in the App Store right now"); return
            }
            let result = try await product.purchase()
            switch result {
            case .success(let verification):
                if var d = describe(verification) {
                    d["status"] = "ok"
                    reply.send(d)
                } else {
                    reply.fail("the App Store's answer could not be verified on this device")
                }
            case .userCancelled:
                reply.send(["status": "cancelled"])
            case .pending:
                reply.send(["status": "pending"])
            @unknown default:
                reply.fail("unexpected App Store result")
            }
        } catch {
            reply.fail("the purchase did not go through: \(error.localizedDescription)")
        }
    }
}

/// Every transaction not yet finished — paid, but not yet acknowledged to Apple because the
/// server has not confirmed the credit. Re-sent to the server on launch and on "Restore".
@_cdecl("tokumai_iap_unfinished")
public func tokumai_iap_unfinished(_ ctx: UnsafeMutableRawPointer?, _ cb: TokumaiIapCallback) {
    let reply = Reply(ctx: ctx, cb: cb)
    guard #available(iOS 15.0, *) else { reply.send(["transactions": []]); return }
    Task {
        var list: [[String: Any]] = []
        for await v in Transaction.unfinished {
            if let d = describe(v) { list.append(d) }
        }
        reply.send(["transactions": list])
    }
}

/// Acknowledge one transaction to Apple — only ever after the server credited it.
/// `status`: ok | gone (already finished).
@_cdecl("tokumai_iap_finish")
public func tokumai_iap_finish(_ idC: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?, _ cb: TokumaiIapCallback) {
    let reply = Reply(ctx: ctx, cb: cb)
    let want = UInt64(cString(idC)) ?? 0
    guard #available(iOS 15.0, *) else { reply.send(["status": "gone"]); return }
    Task {
        for await v in Transaction.unfinished {
            if case .verified(let tx) = v, tx.id == want {
                await tx.finish()
                reply.send(["status": "ok"])
                return
            }
        }
        reply.send(["status": "gone"])
    }
}

/// What the App Store says this Apple ID is currently entitled to — the live subscriptions.
///
/// A renewal is NOT a purchase the app sees: StoreKit charges the card in the background
/// and the app only learns about it the next time it asks. So this is asked on every
/// launch and the answer is handed to the server, which grants the month if it has not
/// granted one yet. Nothing here is trusted on the device: what travels is Apple's own
/// signed JWS, and the server checks it against Apple's root, expiry included.
@_cdecl("tokumai_iap_entitlements")
public func tokumai_iap_entitlements(_ ctx: UnsafeMutableRawPointer?, _ cb: TokumaiIapCallback) {
    let reply = Reply(ctx: ctx, cb: cb)
    guard #available(iOS 15.0, *) else { reply.send(["transactions": []]); return }
    Task {
        var list: [[String: Any]] = []
        for await v in Transaction.currentEntitlements {
            if let d = describe(v) { list.append(d) }
        }
        reply.send(["transactions": list])
    }
}
