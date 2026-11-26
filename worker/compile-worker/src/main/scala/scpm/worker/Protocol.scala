package scpm.worker

import com.github.plokhotnyuk.jsoniter_scala.core.*
import com.github.plokhotnyuk.jsoniter_scala.macros.*

/** Message types of the worker protocol (docs/worker-protocol.md). */
object Protocol:
  val Version = 1

/** A JSON value kept as raw bytes, so envelopes can be decoded before their payload type is known. */
opaque type RawJson = Array[Byte]

object RawJson:
  val EmptyObject: RawJson = "{}".getBytes
  /** Parse errors name the problem and offset only, without jsoniter's hex dump of the input. */
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

/** Just the id of a request, to answer a request that is otherwise malformed. */
final case class RequestId(id: Long)
object RequestId:
  given JsonValueCodec[RequestId] = JsonCodecMaker.make

final case class ResultResponse(id: Long, result: RawJson)
object ResultResponse:
  given JsonValueCodec[ResultResponse] = JsonCodecMaker.make

final case class ErrorBody(code: String, message: String)

/** `id` is null only when the request could not be parsed. */
final case class ErrorResponse(id: Option[Long], error: ErrorBody)
object ErrorResponse:
  given JsonValueCodec[ErrorResponse] = JsonCodecMaker.make(CodecMakerConfig.withTransientNone(false))

final case class Notification(params: Event, method: String = "event")
object Notification:
  given JsonValueCodec[Notification] = JsonCodecMaker.make(CodecMakerConfig.withTransientDefault(false))

// ---------------------------------------------------------------------------------------------
// Methods

final case class InitializeParams(protocolVersion: Int)
object InitializeParams:
  given JsonValueCodec[InitializeParams] = JsonCodecMaker.make

final case class InitializeResult(protocolVersion: Int, workerVersion: String, javaVersion: String, pid: Long)
object InitializeResult:
  given JsonValueCodec[InitializeResult] = JsonCodecMaker.make

final case class CompileParams(
    target: String,
    scalaVersion: String,
    scalaCompilerClasspath: List[String],
    scalaLibraryClasspath: List[String],
    compilerBridge: String,
    sources: List[String],
    classpath: List[String] = Nil,
    classesDir: String,
    analysisFile: String,
    scalacOptions: List[String] = Nil,
    javacOptions: List[String] = Nil,
    javaHome: Option[String] = None,
    rootPaths: Map[String, String] = Map.empty,
    upstreamAnalyses: List[UpstreamAnalysis] = Nil
)

/** An upstream module on the classpath: its classes directory and analysis file (may not exist yet). */
final case class UpstreamAnalysis(classesDir: String, analysisFile: String)
object CompileParams:
  // A list without a default (`sources`, the toolchain classpaths) must be present; `[]` is fine.
  // Otherwise an omitted `sources` would compile nothing and delete every class file.
  given JsonValueCodec[CompileParams] = JsonCodecMaker.make(CodecMakerConfig.withTransientEmpty(false).withRequireCollectionFields(true))

  /** The required lists as sent: jsoniter decodes a JSON `null` list as empty, this as `None`. */
  private final case class RequiredLists(
      sources: Option[List[String]] = None,
      scalaCompilerClasspath: Option[List[String]] = None,
      scalaLibraryClasspath: Option[List[String]] = None
  )
  private given JsonValueCodec[RequiredLists] = JsonCodecMaker.make

  /** Problems with compile params that decoding lets through: a `null` required list. */
  def nullLists(raw: RawJson): List[String] =
    val lists = raw.decode[RequiredLists]
    List(
      "sources" -> lists.sources,
      "scalaCompilerClasspath" -> lists.scalaCompilerClasspath,
      "scalaLibraryClasspath" -> lists.scalaLibraryClasspath
    ).collect { case (field, None) => s"$field: must be an array, not null" }

enum CompileStatus:
  @named("ok") case Ok
  @named("failed") case Failed
  @named("cancelled") case Cancelled
  @named("crashed") case Crashed

final case class CompileResult(
    status: CompileStatus,
    durationMs: Long,
    compiledSources: Int,
    noop: Boolean,
    message: Option[String] = None
)
object CompileResult:
  // Without a discriminator, enum cases are written as plain strings.
  given JsonValueCodec[CompileResult] = JsonCodecMaker.make(CodecMakerConfig.withDiscriminatorFieldName(None))

final case class CancelParams(requestId: Long)
object CancelParams:
  given JsonValueCodec[CancelParams] = JsonCodecMaker.make

final case class Empty()
object Empty:
  given JsonValueCodec[Empty] = JsonCodecMaker.make

// ---------------------------------------------------------------------------------------------
// Events

final case class Pos(line: Int, col: Int)
final case class Range(start: Pos, end: Pos)
final case class TextEdit(file: Option[String], range: Option[Range], newText: String)
final case class Action(title: String, edits: List[TextEdit])

sealed trait Event:
  def requestId: Long
  def target: String

object Event:
  @named("compile.progress")
  final case class Progress(requestId: Long, target: String, phase: String, unitsDone: Int, unitsTotal: Int)
      extends Event

  @named("diagnostic")
  final case class Diagnostic(
      requestId: Long,
      target: String,
      severity: String,
      file: Option[String],
      range: Option[Range],
      code: Option[String],
      message: String,
      rendered: Option[String],
      actions: List[Action]
  ) extends Event

  @named("log")
  final case class Log(requestId: Long, target: String, level: String, message: String) extends Event

  // Unknown values are spelled out as null, as in the protocol examples.
  given JsonValueCodec[Event] = JsonCodecMaker.make(
    CodecMakerConfig
      .withDiscriminatorFieldName(Some("t"))
      .withTransientNone(false)
      .withTransientEmpty(false)
  )
