package scpm.worker

import com.github.plokhotnyuk.jsoniter_scala.core.*

import java.io.OutputStream
import scala.util.control.NonFatal

/** Writes protocol messages as JSON lines. Safe to call from any thread. */
final class Output(out: OutputStream):
  def result[A: JsonValueCodec](id: Long, value: A): Unit =
    try write(ResultResponse(id, RawJson.encode(value)))
    catch case NonFatal(e) => error(Some(id), "internal", s"cannot encode the result: $e")

  def error(id: Option[Long], code: String, message: String): Unit =
    write(ErrorResponse(id, ErrorBody(code, message)))

  def event(e: Event): Unit = write(Notification(e))

  private def write[A: JsonValueCodec](message: A): Unit =
    val bytes = writeToArray(message)
    out.synchronized {
      out.write(bytes)
      out.write('\n')
      out.flush()
    }
