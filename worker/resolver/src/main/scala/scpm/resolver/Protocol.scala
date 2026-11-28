package scpm.resolver

import com.github.plokhotnyuk.jsoniter_scala.core.*
import com.github.plokhotnyuk.jsoniter_scala.macros.*

/** Message types of the resolver sidecar's protocol (design §5.2.1, docs/worker-protocol.md). */
object Protocol:
  val Version = 1

/** A JSON value kept as raw bytes, so envelopes can be decoded before their payload type is known. */
opaque type RawJson = Array[Byte]

object RawJson:
  val EmptyObject: RawJson = "{}".getBytes
  val readerConfig: ReaderConfig = ReaderConfig.withAppendHexDumpToParseException(false)

  def encode[A: JsonValueCodec](value: A): RawJson = writeToArray(value)
  extension (raw: RawJson) def decode[A: JsonValueCodec]: A = readFromArray[A](raw, readerConfig)

  given JsonValueCodec[RawJson] with
    def decodeValue(in: JsonReader, default: RawJson): RawJson = in.readRawValAsBytes()
    def encodeValue(x: RawJson, out: JsonWriter): Unit = out.writeRawVal(x)
    def nullValue: RawJson = EmptyObject

// ---------------------------------------------------------------------------------------------
// Envelopes

final case class Request(id: Long, method: String, params: RawJson = RawJson.EmptyObject)
object Request:
  given JsonValueCodec[Request] = JsonCodecMaker.make

final case class RequestId(id: Long)
object RequestId:
  given JsonValueCodec[RequestId] = JsonCodecMaker.make

final case class ResultResponse(id: Long, result: RawJson)
object ResultResponse:
  given JsonValueCodec[ResultResponse] = JsonCodecMaker.make

/** An error: `code`, a message for people, and structured `data` (design §5.2.1, "Errors"). */
final case class ErrorBody(code: String, message: String, data: Option[RawJson] = None)
object ErrorBody:
  given JsonValueCodec[ErrorBody] = JsonCodecMaker.make

final case class ErrorResponse(id: Option[Long], error: ErrorBody)
object ErrorResponse:
  given JsonValueCodec[ErrorResponse] = JsonCodecMaker.make(CodecMakerConfig.withTransientNone(false))

final case class Empty()
object Empty:
  given JsonValueCodec[Empty] = JsonCodecMaker.make

// ---------------------------------------------------------------------------------------------
// initialize

/** A proxy server, reached over plain HTTP. */
final case class ProxyServer(host: String, port: Int)

/** The proxy settings scpm resolved (design §5.9): per scheme, and hosts reached directly. */
final case class ProxySpec(http: Option[ProxyServer] = None, https: Option[ProxyServer] = None, noProxy: List[String] = Nil)

/** Where trusted roots come from: `pemBundle` on Linux; the OS store on macOS and Windows. */
final case class TrustSpec(pemBundle: Option[String] = None)

final case class InitializeParams(
    protocolVersion: Int,
    proxy: Option[ProxySpec] = None,
    trust: Option[TrustSpec] = None,
    userAgent: Option[String] = None
)
object InitializeParams:
  given JsonValueCodec[InitializeParams] = JsonCodecMaker.make

final case class InitializeResult(
    protocolVersion: Int,
    resolverVersion: String,
    coursierVersion: String,
    javaVersion: String,
    pid: Long
)
object InitializeResult:
  given JsonValueCodec[InitializeResult] = JsonCodecMaker.make

// ---------------------------------------------------------------------------------------------
// Repositories and cache settings, shared by resolve and listVersions

/** Credentials after scpm's plain-HTTP and origin policy; `realm` for a realm-scoped entry. */
final case class AuthSpec(user: String, password: String, realm: Option[String] = None)

/** A configured repository, mirrors already applied to `url`. */
/** `challenges`: realm-scoped credentials from coursier's own sources, each sent only after a
  * challenge naming its realm. */
final case class RepoSpec(
    id: String,
    url: String,
    groups: Option[List[String]] = None,
    auth: Option[AuthSpec] = None,
    challenges: List[AuthSpec] = Nil
)

/** `ttl` in seconds (absent: 24 h); `refresh` re-checks changing metadata whatever the TTL. */
final case class CacheSpec(
    repositories: List[RepoSpec],
    cache: String,
    ttl: Option[Long],
    refresh: Boolean,
    offline: Boolean
)

// ---------------------------------------------------------------------------------------------
// resolve

/** A root: `module` is `org:name`; `exclusions` are `org:name` (either part may be `*`). */
final case class RootSpec(
    module: String,
    version: String,
    classifier: Option[String] = None,
    `type`: Option[String] = None,
    exclusions: List[String] = Nil,
    transitive: Boolean = true
)

/** A native mapping (design §5.4): for `module` at a version inside `versions` (a Maven interval
  * such as `[4.1.0,)`; absent: every version), each system's classifier. */
final case class NativeSpec(module: String, versions: Option[String] = None, natives: Map[String, String] = Map.empty)

final case class ResolveRequestSpec(
    label: String,
    systems: List[String],
    jdk: String,
    natives: List[NativeSpec] = Nil,
    roots: List[RootSpec],
    boms: List[String] = Nil,
    forced: List[String] = Nil,
    artifactTypes: List[String] = Nil
)

final case class ResolveParams(
    repositories: List[RepoSpec],
    cache: String,
    ttl: Option[Long] = None,
    refresh: Boolean = false,
    offline: Boolean = false,
    requests: List[ResolveRequestSpec]
):
  def cacheSpec: CacheSpec = CacheSpec(repositories, cache, ttl, refresh, offline)

object ResolveParams:
  given JsonValueCodec[ResolveParams] = JsonCodecMaker.make

/** A classpath entry. `systems` (sorted) only when it isn't on every system of the request. */
final case class Entry(
    group: String,
    artifact: String,
    version: String,
    classifier: Option[String] = None,
    extension: String,
    url: String,
    file: String,
    repository: String,
    systems: Option[List[String]] = None
)

/** One request's result: `classpath` merged over its systems, or `bySystem` when the merge
  * found a cycle, or `error`. */
final case class RequestResult(
    classpath: Option[List[Entry]] = None,
    bySystem: Option[Map[String, List[Entry]]] = None,
    error: Option[ErrorBody] = None
)

final case class ResolveResult(results: List[RequestResult])
object ResolveResult:
  given JsonValueCodec[ResolveResult] = JsonCodecMaker.make

// ---------------------------------------------------------------------------------------------
// listVersions

final case class ListVersionsParams(
    repositories: List[RepoSpec],
    cache: String,
    ttl: Option[Long] = None,
    refresh: Boolean = false,
    offline: Boolean = false,
    module: String
):
  def cacheSpec: CacheSpec = CacheSpec(repositories, cache, ttl, refresh, offline)

object ListVersionsParams:
  given JsonValueCodec[ListVersionsParams] = JsonCodecMaker.make

final case class RepoVersions(id: String, versions: List[String])
final case class ListVersionsResult(versions: List[String], repositories: List[RepoVersions])
object ListVersionsResult:
  given JsonValueCodec[ListVersionsResult] = JsonCodecMaker.make

// ---------------------------------------------------------------------------------------------
// Error data

final case class Tried(repository: String, url: String, status: String)
final case class NotFoundData(module: String, version: String, tried: List[Tried])
object NotFoundData:
  given JsonValueCodec[NotFoundData] = JsonCodecMaker.make
final case class NoArtifactData(root: String)
object NoArtifactData:
  given JsonValueCodec[NoArtifactData] = JsonCodecMaker.make
final case class UnauthorizedData(repository: String, url: String, status: Int)
object UnauthorizedData:
  given JsonValueCodec[UnauthorizedData] = JsonCodecMaker.make
final case class TransportData(repository: String, url: String, cause: String)
object TransportData:
  given JsonValueCodec[TransportData] = JsonCodecMaker.make
final case class ChecksumData(url: String, expected: String, actual: String)
object ChecksumData:
  given JsonValueCodec[ChecksumData] = JsonCodecMaker.make
final case class OfflineMissData(url: String)
object OfflineMissData:
  given JsonValueCodec[OfflineMissData] = JsonCodecMaker.make
final case class BadEnvironmentData(what: String)
object BadEnvironmentData:
  given JsonValueCodec[BadEnvironmentData] = JsonCodecMaker.make

/** A request failure with its protocol error. */
final class ResolverError(val body: ErrorBody) extends Exception(s"${body.code}: ${body.message}")

object ResolverError:
  def apply[A: JsonValueCodec](code: String, message: String, data: A): ResolverError =
    new ResolverError(ErrorBody(code, message, Some(RawJson.encode(data))))
  def plain(code: String, message: String): ResolverError = new ResolverError(ErrorBody(code, message))
