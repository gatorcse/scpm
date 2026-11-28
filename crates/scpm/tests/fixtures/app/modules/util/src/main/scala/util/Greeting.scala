package util

import cats.syntax.all._

object Greeting {
  def hello(names: List[String]): String = names.map(n => s"Hello, $n").mkString_("", "; ", "!")
  def version: String = Platform.name
}
