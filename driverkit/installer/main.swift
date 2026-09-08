//
//  main.swift
//  Installs and removes the DSD audio driver extension.
//
//  A dext cannot be loaded directly. It has to sit inside an app bundle's
//  Contents/Library/SystemExtensions directory, and that app asks the system to activate it
//  through OSSystemExtensionRequest. This is that app: no interface, one request, and it
//  reports what the system said.
//

import Foundation
import SystemExtensions

let driverBundleIdentifier = "com.github.xenide.dsdrust.driver"

/// Reports what the system decided, and exits once it has decided anything at all.
final class RequestReporter: NSObject, OSSystemExtensionRequestDelegate {
    private let verb: String

    init(verb: String) {
        self.verb = verb
    }

    func request(_ request: OSSystemExtensionRequest,
                 actionForReplacingExtension existing: OSSystemExtensionProperties,
                 withExtension new: OSSystemExtensionProperties) -> OSSystemExtensionRequest.ReplacementAction {
        // Always take the build in this bundle. Version numbers do not move during
        // development, and refusing the replacement would leave the old code loaded.
        print("replacing version \(existing.bundleVersion) with \(new.bundleVersion)")
        return .replace
    }

    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) {
        print("waiting for approval in System Settings > General > Login Items & Extensions")
    }

    func request(_ request: OSSystemExtensionRequest, didFinishWithResult result: OSSystemExtensionRequest.Result) {
        switch result {
        case .completed:
            print("\(verb) completed")
        case .willCompleteAfterReboot:
            print("\(verb) will complete after a reboot")
        @unknown default:
            print("\(verb) finished with an unknown result: \(result.rawValue)")
        }
        exit(0)
    }

    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) {
        let code = (error as NSError).code
        print("\(verb) failed: \(error.localizedDescription) (OSSystemExtensionError \(code))")
        print("`log show --last 2m --predicate 'subsystem == \"com.apple.sysextd\"'` says why")
        exit(1)
    }
}

let arguments = CommandLine.arguments
let action = arguments.count > 1 ? arguments[1] : "activate"

let request: OSSystemExtensionRequest
switch action {
case "activate":
    request = OSSystemExtensionRequest.activationRequest(forExtensionWithIdentifier: driverBundleIdentifier,
                                                         queue: .main)
case "deactivate":
    request = OSSystemExtensionRequest.deactivationRequest(forExtensionWithIdentifier: driverBundleIdentifier,
                                                           queue: .main)
default:
    print("usage: DsdDriverInstaller [activate|deactivate]")
    exit(2)
}

let reporter = RequestReporter(verb: action)
request.delegate = reporter
OSSystemExtensionManager.shared.submitRequest(request)
print("\(action) request submitted for \(driverBundleIdentifier)")
RunLoop.main.run()
