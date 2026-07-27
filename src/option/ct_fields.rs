//! Validation of the CloudTrail field names accepted by `aws-ct-metrics -F`.
//!
//! A name that no event has is not a useful result, it is a typo: `sourceIPaddress` (lowercase
//! `a`) silently produces a table of one `-` row at 100%, which looks exactly like a valid
//! aggregation of a field the logs happen not to carry. Rejecting unknown names at the CLI turns
//! that into an immediate, actionable error — before a scan of a multi-gigabyte export.

/// Top-level record fields whose contents are API-specific, so any path under them is accepted
/// as-is (e.g. `requestParameters.bucketName`, `additionalEventData.MFAUsed`). Only the container
/// itself is checked.
const OPEN_CONTAINERS: &[&str] = &[
    "additionalEventData",
    "addendum",
    "edgeDeviceDetails",
    "insightDetails",
    "requestParameters",
    "resources",
    "responseElements",
    "serviceEventDetails",
];

/// Field names `-F` accepts outright: the top-level members of a CloudTrail record plus the
/// documented paths inside `userIdentity` and `tlsDetails`, whose schema — unlike the containers
/// above — is fixed and is where most DFIR-relevant fields live.
const KNOWN_FIELDS: &[&str] = &[
    // --- top level ---
    "apiVersion",
    "awsRegion",
    "errorCode",
    "errorMessage",
    "eventCategory",
    "eventID",
    "eventName",
    "eventSource",
    "eventTime",
    "eventType",
    "eventVersion",
    "managementEvent",
    "readOnly",
    "recipientAccountId",
    "requestID",
    "sessionCredentialFromConsole",
    "sharedEventID",
    "sourceIPAddress",
    "tlsDetails",
    "userAgent",
    "userIdentity",
    "vpcEndpointAccountId",
    "vpcEndpointId",
    // --- userIdentity ---
    "userIdentity.accessKeyId",
    "userIdentity.accountId",
    "userIdentity.arn",
    "userIdentity.credentialId",
    "userIdentity.identityProvider",
    "userIdentity.inScopeOf",
    "userIdentity.invokedBy",
    "userIdentity.onBehalfOf",
    "userIdentity.onBehalfOf.backTrackId",
    "userIdentity.onBehalfOf.userId",
    "userIdentity.principalId",
    "userIdentity.sessionContext",
    "userIdentity.sessionContext.attributes",
    "userIdentity.sessionContext.attributes.creationDate",
    "userIdentity.sessionContext.attributes.mfaAuthenticated",
    "userIdentity.sessionContext.ec2RoleDelivery",
    "userIdentity.sessionContext.sessionIssuer",
    "userIdentity.sessionContext.sessionIssuer.accountId",
    "userIdentity.sessionContext.sessionIssuer.arn",
    "userIdentity.sessionContext.sessionIssuer.principalId",
    "userIdentity.sessionContext.sessionIssuer.type",
    "userIdentity.sessionContext.sessionIssuer.userName",
    "userIdentity.sessionContext.sourceIdentity",
    "userIdentity.sessionContext.webIdFederationData",
    "userIdentity.sessionContext.webIdFederationData.attributes",
    "userIdentity.sessionContext.webIdFederationData.federatedProvider",
    "userIdentity.type",
    "userIdentity.userName",
    // --- tlsDetails ---
    "tlsDetails.cipherSuite",
    "tlsDetails.clientProvidedHostHeader",
    "tlsDetails.tlsVersion",
];

/// The field names offered when a name is rejected outright — the ones an investigation starts
/// from. Kept short on purpose: the full list is long enough to bury the message.
const SUGGESTED_FIELDS: &str = "eventName, eventSource, sourceIPAddress, userAgent, awsRegion, errorCode, \
     userIdentity.arn, userIdentity.type, userIdentity.accessKeyId";

fn is_known(name: &str) -> bool {
    if KNOWN_FIELDS.contains(&name) {
        return true;
    }
    let container = name.split('.').next().unwrap_or(name);
    OPEN_CONTAINERS.contains(&container)
}

/// The correctly spelled field a mistyped name most likely meant. Only case differences are
/// corrected — that is the mistake this catches (`sourceIPaddress` for `sourceIPAddress`), and
/// guessing at anything looser risks aggregating a field the user did not ask for.
fn suggest(name: &str) -> Option<&'static str> {
    KNOWN_FIELDS
        .iter()
        .chain(OPEN_CONTAINERS.iter())
        .find(|known| known.eq_ignore_ascii_case(name))
        .copied()
}

/// Validate one `-F, --field-name` value. Used as a clap `value_parser`, so an unknown name ends
/// the run at argument parsing, before any log file is read.
pub fn parse_field_name(s: &str) -> Result<String, String> {
    if is_known(s) {
        return Ok(s.to_string());
    }
    match suggest(s) {
        Some(known) => Err(format!(
            "'{s}' is not a CloudTrail field. Did you mean '{known}'? (field names are case-sensitive)"
        )),
        None => Err(format!(
            "'{s}' is not a CloudTrail field. Try one of: {SUGGESTED_FIELDS}. \
             Fields inside {} are also accepted with any sub-path (ex: requestParameters.bucketName).",
            OPEN_CONTAINERS.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_documented_fields() {
        for field in [
            "eventName",
            "sourceIPAddress",
            "userAgent",
            "awsRegion",
            "userIdentity.arn",
            "userIdentity.accessKeyId",
            "userIdentity.sessionContext.attributes.mfaAuthenticated",
            "tlsDetails.tlsVersion",
        ] {
            assert!(parse_field_name(field).is_ok(), "{field}");
        }
    }

    // The contents of these containers depend on the API call, so any sub-path goes through.
    #[test]
    fn accepts_any_path_under_an_open_container() {
        for field in [
            "requestParameters",
            "requestParameters.bucketName",
            "responseElements.credentials.accessKeyId",
            "additionalEventData.MFAUsed",
        ] {
            assert!(parse_field_name(field).is_ok(), "{field}");
        }
    }

    // The case this validation exists for.
    #[test]
    fn rejects_a_miscased_field_and_names_the_right_one() {
        let err = parse_field_name("sourceIPaddress").unwrap_err();
        assert!(err.contains("Did you mean 'sourceIPAddress'"), "{err}");

        let err = parse_field_name("userIdentity.ARN").unwrap_err();
        assert!(err.contains("Did you mean 'userIdentity.arn'"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_field_with_a_starting_point() {
        let err = parse_field_name("sourceIP").unwrap_err();
        assert!(err.contains("is not a CloudTrail field"), "{err}");
        assert!(err.contains("sourceIPAddress"), "{err}");

        // An unknown container is not silently accepted just because it has a dotted path.
        assert!(parse_field_name("requestParameter.bucketName").is_err());
    }
}
