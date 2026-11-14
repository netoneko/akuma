object HelloScala {
  def main(args: Array[String]): Unit = {
    println(s"Hello from Scala ${scala.util.Properties.versionNumberString} on ${sys.props("os.name")} ${sys.props("os.arch")}")
    val xs = (1 to 10).toList
    println(s"squares: ${xs.map(x => x * x)}")
    println(s"sum of evens: ${xs.filter(_ % 2 == 0).sum}")
    case class P(name: String, age: Int)
    println(List(P("ada", 36), P("alan", 41)).sortBy(-_.age))
  }
}
