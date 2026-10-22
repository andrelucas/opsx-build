# Example component contract

## OWN-01 Conditional profile

A verified result includes a profile. Other outcomes may omit it.
The component owns its response construction and tests.

## OWN-02 Finalize

Finalize succeeds only after an active receipt; an absent receipt is refused.
The component must consume the clock provider's public qualification contract.

## DEP-01 Qualified clock

The clock-provider component owns qualification policy and its implementation.
This component consumes that qualification and refuses work when it is lost.

## CONTEXT-01 Other components

Other components own durable recovery policy. This is contextual information.

## TEST-01 Builder observations

Add real assertions for all conditional outcomes, Finalize refusal and success,
and lost clock qualification. Names or skipped tests are insufficient evidence.
