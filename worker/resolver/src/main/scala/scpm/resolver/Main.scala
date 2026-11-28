package scpm.resolver

import com.github.plokhotnyuk.jsoniter_scala.core.*

import java.io.{ BufferedReader, FileDescriptor, FileOutputStream, InputStream, InputStreamReader }
import java.nio.charset.StandardCharsets.UTF_8
import java.util.concurrent.Executors
import scala.jdk.CollectionConverters.*
import scala.util.control.NonFatal

/** The resolver sidecar (design §5.2.1): coursier behind JSON lines on stdin/stdout. */
object Main:
  def main(args: Array[String]): Unit =
    // stdout carries protocol messages only; anything else that prints goes to stderr.
    val protocolOut = FileOutputStream(FileDescriptor.out)
    System.setOut(System.err)
    val bad = Environment.check(System.getProperties.stringPropertyNames.asScala.toSet)
    val code = Server(Output(protocolOut), bad).serve(System.in)
    System.exit(code)

/** Settings the sidecar must not inherit (design §5.2.1, "Start-up check"). */
object Environment:
  /** What is set that must not be, or `None`. */
  def check(properties: Set[String]): Option[String] =
    val found = properties.toList.sorted.filter { p =>
      (p.startsWith("coursier.") && p != "coursier.config-dir") ||
      (p.startsWith("scala-cli.") && p != "scala-cli.config") ||
      p.endsWith(".proxyHost") ||
      p == "java.net.useSystemProxies"
    }
    Option.when(found.nonEmpty)(s"system properties ${found.mkString(", ")} are set")

/**
 * Dispatches requests, each on its own thread: requests run concurrently and every request gets
 * exactly one response. `serve` returns the exit code.
 */
final class Server(out: Output, badEnvironment: Option[String]):
  @volatile private var service: Option[Service] = None
  private val pool = Executors.newCachedThreadPool { r =>
    val t = Thread(r, "scpm-resolver-request")
    t.setDaemon(true)
    t
  }

  def serve(in: InputStream): Int =
    val reader = BufferedReader(InputStreamReader(in, UTF_8))
    var code: Option[Int] = None
    while code.isEmpty do
      val line = reader.readLine()
      if line == null then
        badEnvironment.foreach(what => System.err.println(s"scpm resolver: bad environment: $what"))
        code = Some(if badEnvironment.isDefined then 2 else 0)
      else if line.nonEmpty then
        parse(line).foreach { request =>
          badEnvironment match
            case Some(what) =>
              // Found at start-up, before any request had an id: the first request gets it.
              out.error(Some(request.id), ResolverError("bad_environment", s"the resolver's environment is not clean: $what", BadEnvironmentData(what)).body)
              code = Some(2)
            case None if request.method == "shutdown" =>
              // Requests still running are answered first; then `shutdown`'s own response.
              pool.shutdown()
              pool.awaitTermination(Long.MaxValue, java.util.concurrent.TimeUnit.NANOSECONDS)
              out.result(request.id, Empty())
              code = Some(0)
            case None => handle(request)
        }
    code.get

  private def parse(line: String): Option[Request] =
    def decode[A: JsonValueCodec]: A = readFromString[A](line, RawJson.readerConfig)
    try Some(decode[Request])
    catch
      case e: JsonReaderException =>
        val id =
          try Some(decode[RequestId].id)
          catch case _: JsonReaderException => None
        out.error(id, ErrorBody("bad_request", s"malformed request: ${e.getMessage}"))
        None

  private def handle(request: Request): Unit =
    val id = request.id
    request.method match
      case "initialize" =>
        respond(id) {
          val params = request.params.decode[InitializeParams]
          if params.protocolVersion != Protocol.Version then
            throw ResolverError.plain(
              "unsupported_protocol",
              s"the resolver speaks protocol ${Protocol.Version}, not ${params.protocolVersion}"
            )
          service = Some(Service(params.proxy, params.trust, params.userAgent))
          out.result(
            id,
            InitializeResult(
              Protocol.Version,
              Server.resolverVersion,
              coursier.util.Properties.version,
              Runtime.version.toString,
              ProcessHandle.current.pid
            )
          )
        }
      case "resolve" =>
        pool.execute(() => respond(id)(out.result(id, initialized.resolve(request.params.decode[ResolveParams]))))
      case "listVersions" =>
        pool.execute(() =>
          respond(id)(out.result(id, initialized.listVersions(request.params.decode[ListVersionsParams])))
        )
      case other => out.error(Some(id), ErrorBody("unknown_method", s"unknown method: $other"))

  private def initialized: Service =
    service.getOrElse(throw ResolverError.plain("bad_request", "send `initialize` first"))

  private def respond(id: Long)(body: => Unit): Unit =
    try body
    catch
      case e: ResolverError       => out.error(Some(id), e.body)
      case e: JsonReaderException => out.error(Some(id), ErrorBody("bad_request", e.getMessage))
      case NonFatal(e) =>
        e.printStackTrace()
        out.error(Some(id), ErrorBody("internal", e.toString))

object Server:
  val resolverVersion: String = Option(classOf[Server].getPackage.getImplementationVersion).getOrElse("dev")
