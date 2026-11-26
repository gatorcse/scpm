package hello

@main def run(): Unit =
  val message: String = Greeting.greet("scpm")
  println(message)
