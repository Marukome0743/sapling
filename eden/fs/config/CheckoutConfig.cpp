/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/config/CheckoutConfig.h"

#include <cpptoml.h>

#include <folly/Range.h>
#include <folly/String.h>
#include <folly/io/Cursor.h>
#include <folly/io/IOBuf.h>
#include <folly/json/json.h>
#include <filesystem>
#include <optional>

#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/PathMap.h"
#include "eden/common/utils/SystemError.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/utils/FilterUtils.h"

using folly::ByteRange;
using folly::IOBuf;
using folly::StringPiece;

using namespace std::literals::chrono_literals;

namespace facebook::eden {
namespace {
// TOML config file for the individual client.
const RelativePathPiece kCheckoutConfig{"config.toml"};

// Keys for the TOML config file.
constexpr folly::StringPiece kRepoSection{"repository"};
constexpr folly::StringPiece kRedirectionTargetsSection{"redirection-targets"};
constexpr folly::StringPiece kRepoSourceKey{"path"};
constexpr folly::StringPiece kRepoTypeKey{"type"};
constexpr folly::StringPiece kRepoCaseSensitiveKey{"case-sensitive"};
constexpr folly::StringPiece kMountProtocol{"protocol"};
constexpr folly::StringPiece kInodeCatalogType{"inode-catalog-type"};
constexpr folly::StringPiece kRequireUtf8Path{"require-utf8-path"};
constexpr folly::StringPiece kEnableSqliteOverlay{"enable-sqlite-overlay"};
constexpr folly::StringPiece kUseWriteBackCache{"use-write-back-cache"};
constexpr folly::StringPiece kReCas{"recas"};
constexpr folly::StringPiece kReUseCase{"use-case"};
#ifdef _WIN32
constexpr folly::StringPiece kRepoGuid{"guid"};
constexpr folly::StringPiece kEnableWindowsSymlinks{"enable-windows-symlinks"};
#endif

// Files of interest in the client directory.
const RelativePathPiece kSnapshotFile{"SNAPSHOT"};
const RelativePathPiece kOverlayDir{"local"};
const RelativePathPiece kIntentionallyUnmountedFile{"intentionally-unmounted"};

// File holding mapping of client directories.
const RelativePathPiece kClientDirectoryMap{"config.json"};

// Constants for use with the SNAPSHOT file
//
// - 4 byte identifier: "eden"
// - 4 byte format version number (big endian)
//
// Followed by:
// Version 1:
// - 20 byte commit ID
// - (Optional 20 byte commit ID, only present when there are 2 parents)
// Version 2:
// - 32-bit length
// - Arbitrary-length binary string of said length
// Version 3: (checkout in progress)
// - 32-bit pid of EdenFS process doing the checkout
// - 32-bit length
// - Arbitrary-length binary string of said length for the commit being updated
// from
// - 32-bit length
// - Arbitrary-length binary string of said length for the commit being updated
// to
// Version 4: (Working copy parent and checked out revision)
// - 32-bit length of working copy parent
// - Arbitrary-length binary string of said length for the working copy parent
// - 32-bit length of checked out revision
// - Arbitrary-length binary string of said length for the checked out revision
constexpr folly::StringPiece kSnapshotFileMagic{"eden"};
enum : uint32_t {
  kSnapshotHeaderSize = 8,
  // Legacy SNAPSHOT file version.
  kSnapshotFormatVersion1 = 1,
  // Legacy SNAPSHOT file version.
  kSnapshotFormatVersion2 = 2,
  // State of the SNAPSHOT file when a checkout operation is ongoing.
  kSnapshotFormatCheckoutInProgressVersion = 3,
  // State of the SNAPSHOT file when no checkout operation is ongoing. The
  // SNAPSHOT contains both the currently checked out RootId, as well as the
  // RootId most recently reset to.
  kSnapshotFormatWorkingCopyParentAndCheckedOutRevisionVersion = 4,
};
} // namespace

CheckoutConfig::CheckoutConfig(
    AbsolutePathPiece mountPath,
    AbsolutePathPiece clientDirectory)
    : clientDirectory_(clientDirectory), mountPath_(mountPath) {}

ParentCommit CheckoutConfig::getParentCommit() const {
  // Read the snapshot.
  auto snapshotFile = getSnapshotPath();
  auto snapshotFileContents = readFile(snapshotFile).value();

  StringPiece contents{snapshotFileContents};

  if (contents.size() < kSnapshotHeaderSize) {
    throwf<std::runtime_error>(
        "eden SNAPSHOT file is too short ({} bytes): {}",
        contents.size(),
        snapshotFile);
  }

  if (!contents.startsWith(kSnapshotFileMagic)) {
    throw std::runtime_error("unsupported legacy SNAPSHOT file");
  }

  IOBuf buf(IOBuf::WRAP_BUFFER, ByteRange{contents});
  folly::io::Cursor cursor(&buf);
  cursor += kSnapshotFileMagic.size();
  auto version = cursor.readBE<uint32_t>();
  auto sizeLeft = cursor.length();
  switch (version) {
    case kSnapshotFormatVersion1: {
      if (sizeLeft != Hash20::RAW_SIZE && sizeLeft != (Hash20::RAW_SIZE * 2)) {
        throwf<std::runtime_error>(
            "unexpected length for eden SNAPSHOT file ({} bytes): {}",
            contents.size(),
            snapshotFile);
      }

      Hash20 parent1;
      cursor.pull(parent1.mutableBytes().data(), Hash20::RAW_SIZE);

      if (!cursor.isAtEnd()) {
        // This is never used by EdenFS.
        Hash20 secondParent;
        cursor.pull(secondParent.mutableBytes().data(), Hash20::RAW_SIZE);
      }

      auto rootId = RootId{parent1.toString()};

      // SNAPSHOT v1 stored hashes as binary, but RootId prefers them inflated
      // to human-readable ASCII, so hexlify here.
      return ParentCommit::WorkingCopyParentAndCheckedOutRevision{
          rootId, rootId};
    }

    case kSnapshotFormatVersion2: {
      auto bodyLength = cursor.readBE<uint32_t>();

      // The remainder of the file is the root ID.
      auto rootId = RootId{cursor.readFixedString(bodyLength)};

      return ParentCommit::WorkingCopyParentAndCheckedOutRevision{
          rootId, rootId};
    }

    case kSnapshotFormatCheckoutInProgressVersion: {
      auto pid = cursor.readBE<int32_t>();

      auto fromLength = cursor.readBE<uint32_t>();
      std::string fromRootId = cursor.readFixedString(fromLength);

      auto toLength = cursor.readBE<uint32_t>();
      std::string toRootId = cursor.readFixedString(toLength);

      return ParentCommit::CheckoutInProgress{
          RootId{std::move(fromRootId)}, RootId{std::move(toRootId)}, pid};
    }

    case kSnapshotFormatWorkingCopyParentAndCheckedOutRevisionVersion: {
      auto workingCopyParentLength = cursor.readBE<uint32_t>();
      auto workingCopyParent =
          RootId{cursor.readFixedString(workingCopyParentLength)};

      auto checkedOutLength = cursor.readBE<uint32_t>();
      auto checkedOutRootId = RootId{cursor.readFixedString(checkedOutLength)};

      return ParentCommit::WorkingCopyParentAndCheckedOutRevision{
          std::move(workingCopyParent), std::move(checkedOutRootId)};
    }

    default:
      throwf<std::runtime_error>(
          "unsupported eden SNAPSHOT file format (version {}): {}",
          uint32_t{version},
          snapshotFile);
  }
}

std::optional<std::string> CheckoutConfig::getLastActiveFilter() const {
  if (toBackingStoreType(repoType_) == BackingStoreType::FILTEREDHG) {
    auto parent = getParentCommit().getWorkingCopyParent();
    // It would be better to use the BackingStore to parse the FilterId from the
    // Snapshot file, but the BackingStore is not available to us because the
    // CheckoutConfig is created prior to BackingStore creation. Therefore, we
    // need to use this utility function instead.
    auto [_, filterId] = parseFilterIdFromRootId(parent);
    return filterId;
  } else {
    return std::nullopt;
  }
}

namespace {

constexpr int kNumWriteFileAtomicRetry = 3;

/**
 * Retry writing the passed in file on failure.
 *
 * On Windows, we've seen rare cases where the SNAPSHOT file cannot be written
 * due to a permission denied. This is more than likely caused by an anti-virus
 * software opening the file in an exclusive way and should be transient, thus
 * retrying a couple of times should more than likely succeed.
 */
folly::Try<void> writeFileAtomicWithRetry(
    AbsolutePathPiece path,
    folly::ByteRange content) {
  for (int i = 0; i < kNumWriteFileAtomicRetry - 1; i++) {
    auto ret = writeFileAtomic(path, content);
    if (ret.hasValue()) {
      return ret;
    }
    // TODO(T162069531): This code runs in a Future context, we should ideally
    // futurize all of this to prevent blocking an executor.
    /* sleep override */
    std::this_thread::sleep_for(1ms);
  }
  return writeFileAtomic(path, content);
}

void writeWorkingCopyParentAndCheckedOutRevisision(
    AbsolutePathPiece path,
    const RootId& workingCopy,
    const RootId& checkedOut) {
  const auto& workingCopyString = workingCopy.value();
  XCHECK_LE(workingCopyString.size(), std::numeric_limits<uint32_t>::max());

  const auto& checkedOutString = checkedOut.value();
  XCHECK_LE(checkedOutString.size(), std::numeric_limits<uint32_t>::max());

  auto buf = IOBuf::create(
      kSnapshotHeaderSize + 2 * sizeof(uint32_t) + workingCopyString.size() +
      checkedOutString.size());
  folly::io::Appender cursor{buf.get(), 0};

  // Snapshot file format:
  // 4-byte identifier: "eden"
  cursor.push(ByteRange{kSnapshotFileMagic});
  // 4-byte format version identifier
  cursor.writeBE<uint32_t>(
      kSnapshotFormatWorkingCopyParentAndCheckedOutRevisionVersion);

  // Working copy parent
  cursor.writeBE<uint32_t>(workingCopyString.size());
  cursor.push(folly::StringPiece{workingCopyString});

  // Checked out commit
  cursor.writeBE<uint32_t>(checkedOutString.size());
  cursor.push(folly::StringPiece{checkedOutString});

  writeFileAtomicWithRetry(path, ByteRange{buf->data(), buf->length()}).value();
}
} // namespace

void CheckoutConfig::setCheckedOutCommit(const RootId& commit) const {
  // Pass the same commit for the working copy parent and the checked out
  // commit as a checkout sets both to the same value.
  writeWorkingCopyParentAndCheckedOutRevisision(
      getSnapshotPath(), commit, commit);
}

void CheckoutConfig::setWorkingCopyParentCommit(const RootId& commit) const {
  // The checked out commit doesn't change, reuse what's in the file currently
  auto parentCommit = getParentCommit();
  auto checkedOutRootId =
      parentCommit.getLastCheckoutId(ParentCommit::RootIdPreference::OnlyStable)
          .value();

  writeWorkingCopyParentAndCheckedOutRevisision(
      getSnapshotPath(), commit, checkedOutRootId);
}

void CheckoutConfig::setCheckoutInProgress(const RootId& from, const RootId& to)
    const {
  auto& fromString = from.value();
  auto& toString = to.value();

  auto buf = IOBuf::create(
      kSnapshotHeaderSize + 3 * sizeof(uint32_t) + fromString.size() +
      toString.size());
  folly::io::Appender cursor{buf.get(), 0};

  // Snapshot file format:
  // 4-byte identifier: "eden"
  cursor.push(ByteRange{kSnapshotFileMagic});
  // 4-byte format version identifier
  cursor.writeBE<uint32_t>(kSnapshotFormatCheckoutInProgressVersion);

  // PID of this process
  cursor.writeBE<uint32_t>(getpid());

  // From:
  cursor.writeBE<uint32_t>(fromString.size());
  cursor.push(folly::StringPiece{fromString});

  // To:
  cursor.writeBE<uint32_t>(toString.size());
  cursor.push(folly::StringPiece{toString});

  writeFileAtomicWithRetry(
      getSnapshotPath(), ByteRange{buf->data(), buf->length()})
      .value();
}

void CheckoutConfig::clearIntentionallyUnmountedFlag() const {
  auto intentionallyUnmountedFile =
      clientDirectory_ + kIntentionallyUnmountedFile;
  std::remove(intentionallyUnmountedFile.c_str());
}

bool CheckoutConfig::isIntentionallyUnmounted() const {
  auto intentionallyUnmountedFile =
      clientDirectory_ + kIntentionallyUnmountedFile;
  return std::filesystem::exists(intentionallyUnmountedFile.c_str());
}

const AbsolutePath& CheckoutConfig::getClientDirectory() const {
  return clientDirectory_;
}

std::unique_ptr<std::unordered_map<std::string, std::string>>
CheckoutConfig::getLatestRedirectionTargets() const {
  // Extract repository name from the client config file
  auto configPath = clientDirectory_ + kCheckoutConfig;
  auto configRoot = cpptoml::parse_file(configPath.c_str());
  auto redirection_targets =
      std::make_unique<std::unordered_map<std::string, std::string>>();
  // Load redirection targets
  auto redirectionTargetsTable =
      configRoot->get_table(kRedirectionTargetsSection.str());
  if (redirectionTargetsTable) {
    for (const auto& [path, target] : *redirectionTargetsTable) {
      redirection_targets->emplace(path, target->as<std::string>()->get());
    }
  }

  return redirection_targets;
}

AbsolutePath CheckoutConfig::getSnapshotPath() const {
  return clientDirectory_ + kSnapshotFile;
}

AbsolutePath CheckoutConfig::getOverlayPath() const {
  return clientDirectory_ + kOverlayDir;
}

std::unique_ptr<CheckoutConfig> CheckoutConfig::loadFromClientDirectory(
    AbsolutePathPiece mountPath,
    AbsolutePathPiece clientDirectory) {
  // Extract repository name from the client config file
  auto configPath = clientDirectory + kCheckoutConfig;
  auto configRoot = cpptoml::parse_file(configPath.c_str());

  // Construct CheckoutConfig object
  auto config = std::make_unique<CheckoutConfig>(mountPath, clientDirectory);

  // Load repository information
  auto repository = configRoot->get_table(kRepoSection.str());
  config->repoType_ = *repository->get_as<std::string>(kRepoTypeKey.str());
  config->repoSource_ = *repository->get_as<std::string>(kRepoSourceKey.str());

  FieldConverter<MountProtocol> mountProtocolConverter;
  MountProtocol mountProtocol = kMountProtocolDefault;
  auto mountProtocolStr = repository->get_as<std::string>(kMountProtocol.str());
  if (mountProtocolStr) {
    mountProtocol = mountProtocolConverter.fromString(*mountProtocolStr, {})
                        .value_or(kMountProtocolDefault);
  }
  config->mountProtocol_ = mountProtocol;

  // Read optional case-sensitivity.
  auto caseSensitive = repository->get_as<bool>(kRepoCaseSensitiveKey.str());
  config->caseSensitive_ = caseSensitive
      ? static_cast<CaseSensitivity>(*caseSensitive)
      : kPathMapDefaultCaseSensitive;

  auto requireUtf8Path = repository->get_as<bool>(kRequireUtf8Path.str());
  config->requireUtf8Path_ = requireUtf8Path ? *requireUtf8Path : true;

  FieldConverter<InodeCatalogType> inodeCatalogTypeConverter;
  std::optional<InodeCatalogType> inodeCatalogType;
  auto inodeCatalogTypeStr =
      repository->get_as<std::string>(kInodeCatalogType.str());
  if (inodeCatalogTypeStr) {
    auto result =
        inodeCatalogTypeConverter.fromString(*inodeCatalogTypeStr, {});
    if (result.hasValue()) {
      inodeCatalogType = result.value();
    }
  }
  config->inodeCatalogType_ = inodeCatalogType;

  // TODO(xavierd): Remove the Windows check once D44683911 has been rolled out
  // for several months at which point all the CheckoutConfig will have been
  // rewritten to contain `enable-sqlite-overlay = true`
  if (!folly::kIsWindows) {
    auto enableSqliteOverlay =
        repository->get_as<bool>(kEnableSqliteOverlay.str());
    // SqliteOverlay is default on Windows
    config->enableSqliteOverlay_ =
        enableSqliteOverlay.value_or(folly::kIsWindows);
  }

  auto useWriteBackCache = repository->get_as<bool>(kUseWriteBackCache.str());
  config->useWriteBackCache_ = useWriteBackCache.value_or(false);

  auto recas = configRoot->get_table(kReCas.str());
  if (recas) {
    auto re_use_case = recas->get_as<std::string>(kReUseCase.str());
    if (re_use_case) {
      config->reUseCase_ = *re_use_case;
    }
  }

#ifdef _WIN32
  auto guid = repository->get_as<std::string>(kRepoGuid.str());
  config->repoGuid_ = guid ? Guid{*guid} : Guid::generate();

  auto windowsSymlinksEnabled =
      repository->get_as<bool>(kEnableWindowsSymlinks.str());
  config->enableWindowsSymlinks_ =
      windowsSymlinksEnabled ? *windowsSymlinksEnabled : false;
#endif

  return config;
}

folly::dynamic CheckoutConfig::loadClientDirectoryMap(
    AbsolutePathPiece edenDir) {
  // Extract the JSON and strip any comments.
  auto configJsonFile = edenDir + kClientDirectoryMap;
  auto fileContents = readFile(configJsonFile);

  if (auto* exc = fileContents.tryGetExceptionObject<std::system_error>();
      exc && isEnoent(*exc)) {
    return folly::dynamic::object();
  }
  auto jsonContents = fileContents.value();
  auto jsonWithoutComments = folly::json::stripComments(jsonContents);
  if (jsonWithoutComments.empty()) {
    return folly::dynamic::object();
  }

  // Parse the comment-free JSON while tolerating trailing commas.
  folly::json::serialization_opts options;
  options.allow_trailing_comma = true;
  return folly::parseJson(jsonWithoutComments, options);
}

MountProtocol CheckoutConfig::getMountProtocol() const {
  // NFS is the only mount protocol that we allow to be switched from the
  // default.
  return mountProtocol_ == MountProtocol::NFS ? MountProtocol::NFS
                                              : kMountProtocolDefault;
}

} // namespace facebook::eden
