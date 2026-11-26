package hello

object Greeting:
  def greet(name: String): String = s"Hi, $name!"
  def default(): String = greet("world")
