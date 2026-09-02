// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {FHE, ebool, euint64, externalEuint64} from "@fhevm/solidity/lib/FHE.sol";
import {ZamaConfig} from "@fhevm/solidity/config/ZamaConfig.sol";
import {FHESafeMath} from "@openzeppelin/confidential-contracts/utils/FHESafeMath.sol";
import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {Pausable} from "@openzeppelin/contracts/utils/Pausable.sol";

/// Confidential capacity and exposure for the Solana-Canton bridge.
/// Amounts, limits, and exposure stay encrypted. Only reservation identifiers
/// and the approval bit are disclosed.
contract ConfidentialRiskEngine is AccessControl, Pausable {
    bytes32 public constant POLICY_ADMIN_ROLE = keccak256("POLICY_ADMIN_ROLE");
    bytes32 public constant REQUESTER_ROLE = keccak256("REQUESTER_ROLE");
    bytes32 public constant SETTLER_ROLE = keccak256("SETTLER_ROLE");
    bytes32 public constant PAUSER_ROLE = keccak256("PAUSER_ROLE");

    enum ReservationStatus {
        Empty,
        Reserved,
        Finalized,
        Cancelled,
        Redeemed
    }

    euint64 private globalCapacity;
    euint64 private reservedExposure;
    euint64 private activeExposure;
    uint64 public epoch;

    mapping(bytes32 clientId => euint64 limit) private clientLimit;
    mapping(bytes32 clientId => euint64 usage) private clientUsage;
    mapping(bytes32 clientId => bool configured) private clientConfigured;
    mapping(bytes32 reservationId => ReservationStatus) public reservationStatus;
    mapping(bytes32 reservationId => bytes32) public reservationClient;
    mapping(bytes32 reservationId => euint64) private reservationAmount;
    mapping(bytes32 reservationId => ebool) private reservationApproved;

    event CapacityConfigured(address indexed admin);
    event ClientLimitConfigured(bytes32 indexed clientId);
    event ReservationRecorded(bytes32 indexed reservationId, bytes32 indexed clientId);
    event ReservationFinalized(bytes32 indexed reservationId);
    event ReservationCancelled(bytes32 indexed reservationId);
    event ReservationRedeemed(bytes32 indexed reservationId);
    event EpochRolled(uint64 indexed epoch);

    error DuplicateReservation();
    error UnknownReservation();
    error WrongReservationStatus();
    error ClientNotConfigured();

    constructor(address policyAdmin, address requester, address settler, address pauser) {
        FHE.setCoprocessor(ZamaConfig.getEthereumCoprocessorConfig());
        _grantRole(DEFAULT_ADMIN_ROLE, policyAdmin);
        _grantRole(POLICY_ADMIN_ROLE, policyAdmin);
        _grantRole(REQUESTER_ROLE, requester);
        _grantRole(SETTLER_ROLE, settler);
        _grantRole(PAUSER_ROLE, pauser);
        globalCapacity = FHE.asEuint64(0);
        reservedExposure = FHE.asEuint64(0);
        activeExposure = FHE.asEuint64(0);
        FHE.allowThis(globalCapacity);
        FHE.allowThis(reservedExposure);
        FHE.allowThis(activeExposure);
    }

    function pause() external onlyRole(PAUSER_ROLE) {
        _pause();
    }

    function unpause() external onlyRole(PAUSER_ROLE) {
        _unpause();
    }

    /// Epoch is a policy version. Rolling it does not clear reserved, active, or client usage.
    function rolloverEpoch() external onlyRole(POLICY_ADMIN_ROLE) {
        unchecked {
            epoch += 1;
        }
        emit EpochRolled(epoch);
    }

    function configureCapacity(externalEuint64 encryptedCapacity, bytes calldata inputProof)
        external
        onlyRole(POLICY_ADMIN_ROLE)
    {
        euint64 capacity = FHE.fromExternal(encryptedCapacity, inputProof);
        globalCapacity = capacity;
        FHE.allowThis(globalCapacity);
        emit CapacityConfigured(msg.sender);
    }

    function configureClientLimit(bytes32 clientId, externalEuint64 encryptedLimit, bytes calldata inputProof)
        external
        onlyRole(POLICY_ADMIN_ROLE)
    {
        euint64 limit = FHE.fromExternal(encryptedLimit, inputProof);
        clientLimit[clientId] = limit;
        FHE.allowThis(clientLimit[clientId]);
        if (!clientConfigured[clientId]) {
            clientUsage[clientId] = FHE.asEuint64(0);
            FHE.allowThis(clientUsage[clientId]);
            clientConfigured[clientId] = true;
        }
        emit ClientLimitConfigured(clientId);
    }

    function reserve(bytes32 reservationId, bytes32 clientId, externalEuint64 encryptedAmount, bytes calldata inputProof)
        external
        onlyRole(REQUESTER_ROLE)
        whenNotPaused
    {
        if (reservationStatus[reservationId] != ReservationStatus.Empty) {
            revert DuplicateReservation();
        }
        if (!clientConfigured[clientId]) {
            revert ClientNotConfigured();
        }
        euint64 amount = FHE.fromExternal(encryptedAmount, inputProof);
        (ebool reservedOk, euint64 nextReserved) = FHESafeMath.tryIncrease(reservedExposure, amount);
        (ebool usageOk, euint64 nextUsage) = FHESafeMath.tryIncrease(clientUsage[clientId], amount);
        (ebool totalOk, euint64 nextCommitted) = FHESafeMath.tryAdd(nextReserved, activeExposure);
        ebool withinCapacity = FHE.and(totalOk, FHE.le(nextCommitted, globalCapacity));
        ebool withinClient = FHE.le(nextUsage, clientLimit[clientId]);
        ebool positive = FHE.gt(amount, FHE.asEuint64(0));
        ebool approved = FHE.and(FHE.and(FHE.and(reservedOk, usageOk), FHE.and(withinCapacity, withinClient)), positive);

        reservedExposure = FHE.select(approved, nextReserved, reservedExposure);
        clientUsage[clientId] = FHE.select(approved, nextUsage, clientUsage[clientId]);
        euint64 effectiveAmount = FHE.select(approved, amount, FHE.asEuint64(0));
        reservationAmount[reservationId] = effectiveAmount;
        reservationApproved[reservationId] = approved;
        reservationClient[reservationId] = clientId;
        reservationStatus[reservationId] = ReservationStatus.Reserved;

        FHE.allowThis(reservedExposure);
        FHE.allowThis(clientUsage[clientId]);
        FHE.allowThis(reservationAmount[reservationId]);
        FHE.allowThis(reservationApproved[reservationId]);
        FHE.makePubliclyDecryptable(reservationApproved[reservationId]);

        emit ReservationRecorded(reservationId, clientId);
    }

    function finalize(bytes32 reservationId) external onlyRole(SETTLER_ROLE) whenNotPaused {
        _requireStatus(reservationId, ReservationStatus.Reserved);
        euint64 amount = reservationAmount[reservationId];
        (ebool reservedOk, euint64 nextReserved) = FHESafeMath.tryDecrease(reservedExposure, amount);
        (ebool activeOk, euint64 nextActive) = FHESafeMath.tryIncrease(activeExposure, amount);
        ebool ok = FHE.and(reservedOk, activeOk);
        reservedExposure = FHE.select(ok, nextReserved, reservedExposure);
        activeExposure = FHE.select(ok, nextActive, activeExposure);
        reservationStatus[reservationId] = ReservationStatus.Finalized;
        FHE.allowThis(reservedExposure);
        FHE.allowThis(activeExposure);
        emit ReservationFinalized(reservationId);
    }

    function cancel(bytes32 reservationId) external onlyRole(SETTLER_ROLE) whenNotPaused {
        _requireStatus(reservationId, ReservationStatus.Reserved);
        euint64 amount = reservationAmount[reservationId];
        bytes32 clientId = reservationClient[reservationId];
        (ebool reservedOk, euint64 nextReserved) = FHESafeMath.tryDecrease(reservedExposure, amount);
        (ebool usageOk, euint64 nextUsage) = FHESafeMath.tryDecrease(clientUsage[clientId], amount);
        ebool ok = FHE.and(reservedOk, usageOk);
        reservedExposure = FHE.select(ok, nextReserved, reservedExposure);
        clientUsage[clientId] = FHE.select(ok, nextUsage, clientUsage[clientId]);
        reservationStatus[reservationId] = ReservationStatus.Cancelled;
        FHE.allowThis(reservedExposure);
        FHE.allowThis(clientUsage[clientId]);
        emit ReservationCancelled(reservationId);
    }

    function redeem(bytes32 reservationId) external onlyRole(SETTLER_ROLE) whenNotPaused {
        _requireStatus(reservationId, ReservationStatus.Finalized);
        euint64 amount = reservationAmount[reservationId];
        bytes32 clientId = reservationClient[reservationId];
        (ebool activeOk, euint64 nextActive) = FHESafeMath.tryDecrease(activeExposure, amount);
        (ebool usageOk, euint64 nextUsage) = FHESafeMath.tryDecrease(clientUsage[clientId], amount);
        ebool ok = FHE.and(activeOk, usageOk);
        activeExposure = FHE.select(ok, nextActive, activeExposure);
        clientUsage[clientId] = FHE.select(ok, nextUsage, clientUsage[clientId]);
        reservationStatus[reservationId] = ReservationStatus.Redeemed;
        FHE.allowThis(activeExposure);
        FHE.allowThis(clientUsage[clientId]);
        emit ReservationRedeemed(reservationId);
    }

    function approvalHandle(bytes32 reservationId) external view returns (ebool) {
        return reservationApproved[reservationId];
    }

    function _requireStatus(bytes32 reservationId, ReservationStatus expected) private view {
        if (reservationStatus[reservationId] == ReservationStatus.Empty) {
            revert UnknownReservation();
        }
        if (reservationStatus[reservationId] != expected) {
            revert WrongReservationStatus();
        }
    }
}
